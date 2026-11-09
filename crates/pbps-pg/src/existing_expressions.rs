//! The admission predicate at the expressions that already stand on a table the
//! plan writes rows to or rewrites (#2563, slice 2c, DEC-2563.9).
//!
//! Slices 2a and 2b judge what a plan *introduces*. A row the plan writes also
//! runs what the table already carries — its CHECKs, defaults, generated
//! columns, index keys and filters, the CHECKs and DEFAULT of its columns'
//! domains, and the policies that apply to the deploying role — and runs it as
//! that role. This module is the one place that decides which of those a given
//! statement runs, and judges them.
//!
//! # Which sites a statement runs
//!
//! Measured on PostgreSQL 16.15 and 18.6 (identical), by advancing a sequence
//! from an IMMUTABLE function at each site:
//!
//! | statement | runs |
//! |---|---|
//! | `INSERT` | CHECK, generated, every index key and filter, domain CHECKs, and the DEFAULT of a column the statement omits |
//! | `UPDATE` | CHECK, every index key and filter (HOT skipping is data-dependent, so all are judged), generated and domain CHECKs of the columns it sets |
//! | `DELETE`, `TRUNCATE` | none |
//! | a referential action | the referencing table's `UPDATE` (the DEFAULT of a column `SET DEFAULT` sets); a cascaded `DELETE` runs none |
//! | a rewrite, `REINDEX`, `ANALYZE` | index keys and filters only |
//!
//! A policy runs only where row security applies to the deploying role, which
//! is the server's own answer (`row_security_active`); a referential action
//! bypasses it (DEC-2563.10).
//!
//! The trees are read from the catalog as stored and judged without binding:
//! [`crate::input_path::judge_trees`] takes the text of `conbin`, `adbin`,
//! `indexprs`, `indpred`, `polqual`, `polwithcheck` and a domain's `conbin` and
//! `typdefaultbin`. The tables come from the reference-data guard's closure
//! ([`crate::data_triggers`]), which already follows cascades, partitions and
//! inheritance and locks what it reaches.

use std::collections::BTreeMap;

use pbps_db::Conn;
use pbps_model::{Change, TableName};

use crate::admission::Admission;
use crate::estimate::Rewrite;
use crate::input_path::{Part, Use, judge_trees};
use pbps_dialect::{ColumnDomain, RewriteScope};

/// The kind of expression a site holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Check,
    Default,
    Generated,
    IndexKey,
    IndexFilter,
    Policy,
    DomainCheck,
    DomainDefault,
}

impl Kind {
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "check" => Self::Check,
            "default" => Self::Default,
            "generated" => Self::Generated,
            "index_key" => Self::IndexKey,
            "index_filter" => Self::IndexFilter,
            "policy" => Self::Policy,
            "domain_check" => Self::DomainCheck,
            "domain_default" => Self::DomainDefault,
            _ => return None,
        })
    }
}

/// One stored expression and where it stands.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Site {
    pub kind: Kind,
    /// The table's oid, or the domain's for a domain site: two reads of the same
    /// expression through different statements are one site.
    pub oid: i64,
    /// Where it stands, as a refusal shows it: `app.t`, or the domain's name.
    pub on: String,
    /// The constraint, index or policy; the column for a default or a generated
    /// expression.
    pub name: String,
    pub tree: String,
}

impl Site {
    /// The phrase a refusal uses for the site.
    pub(crate) fn describe(&self) -> String {
        let name = &self.name;
        let on = &self.on;
        match self.kind {
            Kind::Check => format!("the existing CHECK `{name}` of {on}"),
            Kind::Default => format!("the existing DEFAULT of the column `{name}` of {on}"),
            Kind::Generated => {
                format!("the existing generated expression of the column `{name}` of {on}")
            }
            Kind::IndexKey => format!("the existing index `{name}` of {on} (its keys)"),
            Kind::IndexFilter => format!("the existing index `{name}` of {on} (its filter)"),
            Kind::Policy => format!("the existing policy `{name}` of {on}"),
            Kind::DomainCheck => format!("the existing CHECK `{name}` of the domain {on}"),
            Kind::DomainDefault => format!("the existing DEFAULT of the domain {on}"),
        }
    }
}

/// Judges every routine the sites call, by the one predicate, all at once: a
/// refusal names every routine that is not admitted, each with its fix.
///
/// # Errors
/// The refusal, with the remedies; nothing has been changed.
pub(crate) async fn judge(
    conn: &mut Conn,
    admission: &Admission,
    sites: Vec<Site>,
) -> Result<(), String> {
    let mut unique: BTreeMap<(Kind, i64, String), Site> = BTreeMap::new();
    for site in sites {
        unique
            .entry((site.kind, site.oid, site.name.clone()))
            .or_insert(site);
    }
    if unique.is_empty() {
        return Ok(());
    }
    let trees = unique
        .into_values()
        .map(|site| {
            (
                Use {
                    ty: site.describe(),
                    part: Part::TableExpression,
                },
                site.tree,
            )
        })
        .collect();
    judge_trees(conn, admission, trees, false, &|why: &str| {
        format!("the expressions already on a table this plan writes {why}")
    })
    .await
    .map(|_| ())
    .map_err(|why| {
        format!(
            "this plan writes rows that run expressions already on their tables, as the \
             deploying role: {why}"
        )
    })
}

/// The row write a change that rewrites or scans a table's rows amounts to, for
/// the closure: the table and what it does to the rows. `None` for a change
/// that touches no stored row.
///
/// Whether a change rewrites is [`crate::estimate::estimate`]'s to say, for every
/// change: a `Rewrite::Yes` or an `Unknown` is judged, and only a `No` is free,
/// since the answer to "does it rewrite" is exactly what `estimate` cannot
/// always give (DEC-2563.9). No list of change kinds is kept here (#2775): a
/// kind this module forgot to list ran its indexes as the deploying role
/// unjudged. The kinds `estimate` has no answer for and that touch rows are
/// named, each with the reason: the rebuild's copy and `ANALYZE`, which are the
/// statements and not estimates, and the retype of an enum or domain rebuild,
/// which `estimate` leaves out in error.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "every other change is classified by `estimate`, which matches exhaustively"
)]
pub fn rewrite_write(change: &Change) -> Option<pbps_dialect::RowWrite> {
    use pbps_dialect::{RowOperation, RowWrite};
    match change {
        Change::CopyTableRows(copy) => Some(RowWrite {
            table: copy.target.name.clone(),
            operation: RowOperation::Copy {
                // The columns its `INSERT ... SELECT` lists.
                columns: copy
                    .columns
                    .iter()
                    .map(|column| column.target.name.clone())
                    .collect(),
            },
        }),
        Change::AnalyzeTableTree(tree) => Some(RowWrite {
            table: tree.parent.name.clone(),
            operation: RowOperation::Rewrite {
                scope: RewriteScope::Indexes,
            },
        }),
        // `estimate` calls every `EnumRebuild` step "not about a table's stored
        // rows", which is true of the type steps and false of the retype: it is
        // an `ALTER COLUMN TYPE` with `USING`, a rewrite that rebuilds the
        // indexes and revalidates the CHECKs that mention the column (measured
        // on 16 and 18, DEC-2563.9). The column becomes the rebuilt type.
        Change::EnumRebuild {
            tmp,
            step: pbps_model::EnumRebuildStep::Retype { column, .. },
            ..
        } => Some(RowWrite {
            table: column.table.clone(),
            operation: RowOperation::Rewrite {
                scope: RewriteScope::Column {
                    column: column.name.clone(),
                    domain: ColumnDomain::Becomes(tmp.clone()),
                },
            },
        }),
        _ => {
            let estimate = crate::estimate::estimate(change, pbps_model::Strategy::default())?;
            (!matches!(estimate.rewrite, Rewrite::No)).then_some(RowWrite {
                table: estimate.table,
                operation: RowOperation::Rewrite {
                    scope: scope_of(change),
                },
            })
        }
    }
}

/// What a rewrite re-evaluates besides the indexes, measured on 16 and 18
/// (DEC-2563.9). A kind not named here is judged as a row insert is: a rewrite
/// nobody measured is not a free one.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "every kind not measured is judged in full"
)]
fn scope_of(change: &Change) -> RewriteScope {
    match change {
        // The CHECKs that mention the column, and the domain it has after.
        Change::AlterColumnType { column, to, .. } => RewriteScope::Column {
            column: column.name.clone(),
            domain: to
                .type_ref()
                .map_or(ColumnDomain::Leaves, |ty| ColumnDomain::Becomes(ty.clone())),
        },
        Change::AlterColumnExpression { column, .. } => RewriteScope::Column {
            column: column.name.clone(),
            domain: ColumnDomain::Unchanged,
        },
        // A column of a domain: the domain's CHECK may read every row, which
        // is what `estimate` says of it.
        Change::AddColumn { name, column, .. } => match column.ty.type_ref() {
            Some(ty) => RewriteScope::Column {
                column: name.clone(),
                domain: ColumnDomain::Becomes(ty.clone()),
            },
            None => RewriteScope::Indexes,
        },
        Change::SetTablePersistence { .. } => RewriteScope::Indexes,
        _ => RewriteScope::Unmeasured,
    }
}

/// The table a rewrite change names, as the plan spells it.
#[must_use]
pub fn rewritten_table(change: &Change) -> Option<TableName> {
    rewrite_write(change).map(|write| write.table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(kind: Kind, oid: i64, name: &str) -> Site {
        Site {
            kind,
            oid,
            on: "app.t".to_owned(),
            name: name.to_owned(),
            tree: "{}".to_owned(),
        }
    }

    #[test]
    fn every_kind_has_its_own_sentence() {
        let all = [
            Kind::Check,
            Kind::Default,
            Kind::Generated,
            Kind::IndexKey,
            Kind::IndexFilter,
            Kind::Policy,
            Kind::DomainCheck,
            Kind::DomainDefault,
        ];
        let said: std::collections::BTreeSet<String> =
            all.iter().map(|k| site(*k, 1, "x").describe()).collect();
        assert_eq!(said.len(), all.len(), "{said:?}");
        assert!(said.iter().all(|s| s.starts_with("the existing ")));
    }

    fn alter(from: &str, to: &str) -> Change {
        Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: "app.t.v".parse().unwrap(),
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        }
    }

    fn add(source: pbps_model::ValueSource) -> Change {
        let mut column = pbps_model::Column::new("integer".parse().unwrap());
        column.value_source = source;
        Change::AddColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            name: "v".into(),
            column: Box::new(column),
        }
    }

    #[test]
    fn only_a_change_that_rewrites_or_might_is_a_rewrite_write() {
        use pbps_dialect::RowOperation;
        // `integer` to `bigint` rewrites every row, and so re-runs the indexes.
        let write = rewrite_write(&alter("integer", "bigint")).expect("a rewrite");
        assert_eq!(write.table, "app.t".parse().unwrap());
        assert_eq!(
            write.operation,
            RowOperation::Rewrite {
                scope: RewriteScope::Column {
                    column: "v".into(),
                    domain: ColumnDomain::Leaves,
                }
            }
        );
        // Widening a bound in place touches no row.
        assert_eq!(
            rewrite_write(&alter("character varying(10)", "character varying(20)")),
            None
        );
        // A literal default writes no row; a stored generated column writes all.
        assert_eq!(
            rewrite_write(&add(pbps_model::ValueSource::Default("7".into()))),
            None
        );
        assert_eq!(rewrite_write(&add(pbps_model::ValueSource::None)), None);
        assert!(
            rewrite_write(&add(pbps_model::ValueSource::Generated(
                pbps_model::Generated {
                    expression: "a * 2".into(),
                    stored: true,
                }
            )))
            .is_some()
        );
        // A default the tool cannot prove constant might: not free.
        assert!(
            rewrite_write(&add(pbps_model::ValueSource::Default(
                "gen_random_uuid()".into()
            )))
            .is_some()
        );
    }

    /// Whether `rewrite_write` is `estimate`'s answer for every change `estimate`
    /// classifies (#2775): `Some` exactly when the rewrite is not `No`, on the
    /// table `estimate` names, whatever the kind. `estimate` matches every change
    /// without a wildcard, so a kind added later is classified there, and this
    /// module keeps no list that could fall behind.
    #[test]
    fn rewrite_write_is_the_estimates_answer_for_every_change_it_classifies() {
        use pbps_model::{ColumnType, ValueSource};
        let uid: pbps_model::Uid = "c_dddddd".parse().unwrap();
        let column: pbps_model::ColumnRef = "app.t.v".parse().unwrap();
        let table: TableName = "app.t".parse().unwrap();
        let samples: Vec<(&str, Change, bool)> = vec![
            ("a retype that rebuilds", alter("integer", "bigint"), true),
            (
                "a bound widened in place",
                alter("character varying(10)", "character varying(20)"),
                false,
            ),
            (
                "a stored generated column",
                add(ValueSource::Generated(pbps_model::Generated {
                    expression: "a * 2".into(),
                    stored: true,
                })),
                true,
            ),
            (
                "a default estimate cannot prove constant",
                add(ValueSource::Default("gen_random_uuid()".into())),
                true,
            ),
            (
                "a literal default",
                add(ValueSource::Default("7".into())),
                false,
            ),
            ("a column with no source", add(ValueSource::None), false),
            (
                "a column of a domain, whose CHECK may read every row",
                {
                    let mut domain =
                        pbps_model::Column::new(ColumnType::reference("app.dm".parse().unwrap()));
                    domain.value_source = ValueSource::None;
                    Change::AddColumn {
                        uid: uid.clone(),
                        table: table.clone(),
                        name: "d".into(),
                        column: Box::new(domain),
                    }
                },
                true,
            ),
            (
                "a persistence switch",
                Change::SetTablePersistence {
                    uid: uid.clone(),
                    table: table.clone(),
                    unlogged: true,
                },
                true,
            ),
            (
                "a stored generated expression changed in place (PostgreSQL 17 and later)",
                Change::AlterColumnExpression {
                    uid: uid.clone(),
                    column: column.clone(),
                    from: "a".into(),
                    to: "b".into(),
                },
                true,
            ),
            (
                "a default changed",
                Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: column.clone(),
                    from: Some("1".into()),
                    to: None,
                },
                false,
            ),
            (
                "a column dropped",
                Change::DropColumn {
                    uid: uid.clone(),
                    column: column.clone(),
                },
                false,
            ),
            (
                "a column renamed",
                Change::RenameColumn {
                    uid: uid.clone(),
                    table: table.clone(),
                    from: "v".into(),
                    to: "w".into(),
                    table_was: None,
                },
                false,
            ),
            (
                "a replica identity",
                Change::SetReplicaIdentity {
                    uid: uid.clone(),
                    table: table.clone(),
                    to: None,
                },
                false,
            ),
        ];
        for (label, change, rewrites) in samples {
            let estimate =
                crate::estimate::estimate(&change, pbps_model::Strategy::default()).expect(label);
            // Pins the classification the test relies on, so that the day
            // `estimate` changes its mind the test says so.
            assert_eq!(
                !matches!(estimate.rewrite, Rewrite::No),
                rewrites,
                "{label}"
            );
            let write = rewrite_write(&change);
            assert_eq!(write.is_some(), rewrites, "{label}");
            // The emitter tags the first statement with it, and that statement is
            // one the guard can hold a lock across: a staged apply checks only a
            // transactional statement before it runs.
            if rewrites {
                let statements =
                    crate::emit::emit(&crate::Postgres::new(), &change, Default::default())
                        .expect(label);
                let first = statements.first().expect(label);
                assert_eq!(first.rewrites, write, "{label}");
                assert!(first.transactional, "{label}");
            }
            if let Some(write) = write {
                assert_eq!(write.table, estimate.table, "{label}");
                assert!(
                    matches!(write.operation, pbps_dialect::RowOperation::Rewrite { .. }),
                    "{label}"
                );
            }
        }
        // A change `estimate` has no answer for touches no stored row it
        // estimates: a grant, a module, a row of the plan's own.
        let grant = Change::Grant {
            role: "app_reader".into(),
            target: "app.t".parse().unwrap(),
            permissions: Default::default(),
        };
        assert!(crate::estimate::estimate(&grant, pbps_model::Strategy::default()).is_none());
        assert_eq!(rewrite_write(&grant), None);
    }

    /// What each rewrite re-evaluates besides the indexes, measured on 16 and 18
    /// (DEC-2563.9); a kind nobody measured is judged in full.
    #[test]
    fn a_rewrite_names_what_it_validates_and_an_unmeasured_one_is_judged_in_full() {
        use pbps_model::{ColumnType, ValueSource};
        let scope = |change: &Change| {
            let operation = rewrite_write(change).expect("a rewrite").operation;
            let pbps_dialect::RowOperation::Rewrite { scope } = operation else {
                panic!("not a rewrite: {operation:?}");
            };
            scope
        };
        let uid: pbps_model::Uid = "c_eeeeee".parse().unwrap();
        let column = |domain| RewriteScope::Column {
            column: "v".into(),
            domain,
        };
        // A retype validates the CHECKs that mention the column, and the domain
        // it is changed to; to a type that is no domain, none.
        assert_eq!(
            scope(&alter("integer", "app.dm")),
            column(ColumnDomain::Becomes("app.dm".parse().unwrap()))
        );
        assert_eq!(
            scope(&alter("integer", "bigint")),
            column(ColumnDomain::Leaves)
        );
        // `SET EXPRESSION` recomputes the column in place, so the domain it has
        // now validates the new values.
        let in_place = Change::AlterColumnExpression {
            uid: uid.clone(),
            column: "app.t.v".parse().unwrap(),
            from: "a".into(),
            to: "b".into(),
        };
        assert_eq!(scope(&in_place), column(ColumnDomain::Unchanged));
        // A kind nobody measured is judged in full.
        let dropped = Change::DropColumn {
            uid: "c_aaaaab".parse().unwrap(),
            column: "app.t.v".parse().unwrap(),
        };
        assert_eq!(scope_of(&dropped), RewriteScope::Unmeasured);
        // A persistence switch and an ordinary `ADD COLUMN` rebuild the indexes alone.
        let persistence = Change::SetTablePersistence {
            uid,
            table: "app.t".parse().unwrap(),
            unlogged: true,
        };
        assert_eq!(scope(&persistence), RewriteScope::Indexes);
        assert_eq!(
            scope(&add(ValueSource::Generated(pbps_model::Generated {
                expression: "a * 2".into(),
                stored: true,
            }))),
            RewriteScope::Indexes
        );
        // A column of a domain: the domain's CHECK may read every row.
        let mut of_a_domain =
            pbps_model::Column::new(ColumnType::reference("app.dm".parse().unwrap()));
        of_a_domain.value_source = ValueSource::None;
        let added = Change::AddColumn {
            uid: "c_ffffff".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            name: "d".into(),
            column: Box::new(of_a_domain),
        };
        assert_eq!(
            scope(&added),
            RewriteScope::Column {
                column: "d".into(),
                domain: ColumnDomain::Becomes("app.dm".parse().unwrap()),
            }
        );
    }

    #[test]
    fn the_retype_of_a_type_rebuild_is_a_rewrite_though_estimate_has_no_answer_for_it() {
        let retype = Change::EnumRebuild {
            uid: "y_000001".parse().unwrap(),
            name: "app.mood".parse().unwrap(),
            tmp: "app.pbps_rebuild_y_000001".parse().unwrap(),
            step: pbps_model::EnumRebuildStep::Retype {
                column_uid: "c_000001".parse().unwrap(),
                column: "app.t.v".parse().unwrap(),
                to: pbps_model::ColumnType::reference("app.pbps_rebuild_y_000001".parse().unwrap()),
                array: false,
                values: Box::new(pbps_model::ValueChange::Relabel { removed: vec![] }),
            },
        };
        assert!(crate::estimate::estimate(&retype, pbps_model::Strategy::default()).is_none());
        let write = rewrite_write(&retype).expect("the retype rewrites the table");
        assert_eq!(write.table, "app.t".parse().unwrap());
        assert_eq!(
            write.operation,
            pbps_dialect::RowOperation::Rewrite {
                scope: RewriteScope::Column {
                    column: "v".into(),
                    domain: ColumnDomain::Becomes("app.pbps_rebuild_y_000001".parse().unwrap()),
                }
            }
        );
        // The emitter tags its statement, and the guard can hold a lock across it.
        let statements = crate::emit::emit(&crate::Postgres::new(), &retype, Default::default())
            .expect("the retype emits");
        let first = statements.first().expect("a statement");
        assert_eq!(first.rewrites, Some(write));
        assert!(first.transactional);
        // The type steps touch no row.
        let create = Change::EnumRebuild {
            uid: "y_000001".parse().unwrap(),
            name: "app.mood".parse().unwrap(),
            tmp: "app.pbps_rebuild_y_000001".parse().unwrap(),
            step: pbps_model::EnumRebuildStep::Swap {
                definition: Box::new(pbps_model::ManagedType::Enum {
                    values: vec!["a".into()],
                }),
                comment: None,
                depth: 0,
                over: None,
            },
        };
        assert_eq!(rewrite_write(&create), None);
    }

    #[test]
    fn the_first_statement_of_a_rewrite_carries_the_write_the_executor_judges() {
        let pg = crate::Postgres::new();
        let statements = crate::emit::emit(&pg, &alter("integer", "bigint"), Default::default())
            .expect("the change emits");
        let first = statements.first().expect("a statement");
        assert!(first.rewrites.is_some(), "{first:?}");
        assert!(first.row_write.is_none(), "it is no reference-data write");
        assert!(
            statements.iter().skip(1).all(|s| s.rewrites.is_none()),
            "only the first, so it is judged once"
        );
        // A change that rewrites nothing carries nothing.
        let plain = crate::emit::emit(&pg, &add(pbps_model::ValueSource::None), Default::default())
            .expect("the change emits");
        assert!(plain.iter().all(|s| s.rewrites.is_none()));
    }

    #[test]
    fn a_row_change_or_a_drop_is_not_a_rewrite_write() {
        let drop = Change::DropColumn {
            uid: "c_cccccc".parse().unwrap(),
            column: "app.t.v".parse().unwrap(),
        };
        assert_eq!(rewrite_write(&drop), None);
        assert_eq!(rewritten_table(&drop), None);
        assert_eq!(
            rewritten_table(&alter("integer", "bigint")),
            Some("app.t".parse().unwrap())
        );
    }

    #[test]
    fn labels_round_trip_to_kinds() {
        for (label, kind) in [
            ("check", Kind::Check),
            ("default", Kind::Default),
            ("generated", Kind::Generated),
            ("index_key", Kind::IndexKey),
            ("index_filter", Kind::IndexFilter),
            ("policy", Kind::Policy),
            ("domain_check", Kind::DomainCheck),
            ("domain_default", Kind::DomainDefault),
        ] {
            assert_eq!(Kind::from_label(label), Some(kind));
        }
        assert_eq!(Kind::from_label("trigger"), None);
    }
}
