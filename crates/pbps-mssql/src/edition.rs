//! The server edition, and the risks that depend on it (ADR-0003 decision 3).
//!
//! # Why this is not part of the emitter
//!
//! The same DDL carries different consequences on different editions of the
//! same engine: `WITH (ONLINE = ON)` is Enterprise-only, and adding a NOT NULL
//! column with a DEFAULT is metadata-only on Enterprise but a size-of-data
//! rewrite on Standard. None of that is knowable offline, so the emitter writes
//! the statement the strategy asked for and `plan --db` — which has a
//! connection, and therefore a fact instead of a guess — decides whether this
//! server will accept it.
//!
//! An offline plan assumes the conservative edition and says so, rather than
//! promising something it cannot check.

use pbps_db::{Conn, DbError};
use pbps_model::{Change, ChangeSet};

use crate::catalog::get;

/// The editions that differ in ways this tool has to care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edition {
    /// Enterprise, Developer, Evaluation, and Azure SQL Database — the full
    /// feature set as far as online DDL is concerned.
    Full(String),
    /// Standard, Web, Express: no online index operations.
    Limited(String),
    /// `SERVERPROPERTY('Edition')` returned something unrecognised. Treated as
    /// limited, because assuming a feature exists is the failure that costs a
    /// deployment window.
    Unknown(String),
}

impl Edition {
    /// What the server called itself, verbatim — the string an operator will
    /// recognise from their licence.
    pub fn name(&self) -> &str {
        match self {
            Edition::Full(s) | Edition::Limited(s) | Edition::Unknown(s) => s,
        }
    }

    /// Whether `WITH (ONLINE = ON)` will be accepted.
    pub fn supports_online(&self) -> bool {
        matches!(self, Edition::Full(_))
    }

    /// Classifies the string `SERVERPROPERTY('Edition')` returns.
    ///
    /// Matching on substrings rather than an exact list: the strings carry
    /// architecture suffixes ("Enterprise Edition: Core-based Licensing
    /// (64-bit)") that change between releases, and a plan must not start
    /// refusing ONLINE because Microsoft added a word.
    pub fn classify(raw: &str) -> Self {
        let lower = raw.to_ascii_lowercase();
        let full = [
            "enterprise",
            "developer",
            "evaluation",
            // Azure SQL Database and Managed Instance both do online DDL.
            "sql azure",
            "azure sql",
        ];
        let limited = ["standard", "express", "web", "business intelligence"];
        if full.iter().any(|k| lower.contains(k)) {
            Edition::Full(raw.trim().to_owned())
        } else if limited.iter().any(|k| lower.contains(k)) {
            Edition::Limited(raw.trim().to_owned())
        } else {
            Edition::Unknown(raw.trim().to_owned())
        }
    }
}

/// Asks the server which edition it is.
pub async fn edition(conn: &mut Conn) -> Result<Edition, DbError> {
    let rows = conn
        .query("SELECT CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition;")
        .await?;
    let raw: &str = match rows.first() {
        Some(row) => get(row, "edition")?,
        None => return Err(DbError::BadRow("`edition` returned no row".into())),
    };
    Ok(Edition::classify(raw))
}

/// Changes whose declared strategy this edition cannot carry out.
///
/// Returned rather than warned about: emitting `WITH (ONLINE = ON)` against
/// Standard fails at the statement, halfway through an apply. Failing at plan
/// time, with the edition named, is the same information at the only moment it
/// is still cheap.
///
/// # Why the declared strategy is not enough on its own
///
/// `strategy:` is persistent (ADR-0003): it sits on the table and applies to
/// every change to it, including the many for which the emitter deliberately
/// writes no ONLINE clause — adding a column, adding a foreign key or a check,
/// creating the table in the first place. Refusing on the annotation alone
/// would mean that a Standard-edition environment could not plan an ordinary
/// metadata change against any table somebody had annotated, which is a
/// refusal with nothing behind it. Only a change whose statements really carry
/// the clause is one this edition would reject.
pub fn online_not_supported(changes: &ChangeSet, edition: &Edition) -> Vec<String> {
    if edition.supports_online() {
        return Vec::new();
    }
    changes
        .changes
        .iter()
        .filter(|p| p.strategy.online && crate::emit::takes_online(&p.change))
        .map(|p| p.change.subject())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Advisories that are true of this edition but block nothing.
///
/// Adding a NOT NULL column with a DEFAULT rewrites every row on Standard and
/// is metadata-only on Enterprise. Both succeed; only one of them takes the
/// table out of service for an hour, and the approver at the deployment gate is
/// the person who needs to know which.
pub fn size_of_data_warnings(changes: &ChangeSet, edition: &Edition) -> Vec<String> {
    if edition.supports_online() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for p in &changes.changes {
        if let Change::AddColumn {
            table,
            name,
            column,
            ..
        } = &p.change
            && !column.nullable
            && column.default.is_some()
        {
            out.push(format!(
                "{table}.{name}: adding a NOT NULL column with a DEFAULT rewrites every row on \
                 {} (it is metadata-only on Enterprise)",
                edition.name()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_full_editions_are_recognised_with_their_suffixes() {
        for raw in [
            "Enterprise Edition: Core-based Licensing (64-bit)",
            "Developer Edition (64-bit)",
            "SQL Azure",
        ] {
            assert!(
                Edition::classify(raw).supports_online(),
                "{raw} should support ONLINE"
            );
        }
    }

    #[test]
    fn the_limited_editions_are_recognised() {
        for raw in ["Standard Edition (64-bit)", "Express Edition (64-bit)"] {
            let e = Edition::classify(raw);
            assert!(!e.supports_online(), "{raw}");
            assert_eq!(e.name(), raw);
        }
    }

    /// An unrecognised edition must not be optimistically treated as full: the
    /// cost of guessing wrong is a statement that fails mid-apply.
    #[test]
    fn an_unknown_edition_is_treated_as_limited() {
        let e = Edition::classify("Hyperscale Turbo Edition");
        assert!(matches!(e, Edition::Unknown(_)));
        assert!(!e.supports_online());
    }

    fn add_column(nullable: bool, default: Option<&str>) -> pbps_model::PlannedChange {
        let mut column = pbps_model::Column::new("int".parse().unwrap());
        column.nullable = nullable;
        column.default = default.map(str::to_owned);
        pbps_model::PlannedChange::new(Change::AddColumn {
            uid: "c_a1b2c3".parse().unwrap(),
            table: "dbo.customer".parse().unwrap(),
            name: "score".into(),
            column: Box::new(column),
        })
    }

    /// `ALTER COLUMN` is one of the statements the emitter really writes
    /// `WITH (ONLINE = ON)` on.
    fn alter_nullability() -> pbps_model::PlannedChange {
        pbps_model::PlannedChange::new(Change::AlterColumnNullability {
            uid: "c_a1b2c3".parse().unwrap(),
            column: "dbo.customer.score".parse().unwrap(),
            ty: "int".parse().unwrap(),
            to_nullable: true,
        })
    }

    fn online(c: pbps_model::PlannedChange) -> pbps_model::PlannedChange {
        c.with_strategy(pbps_model::Strategy { online: true })
    }

    #[test]
    fn a_full_edition_blocks_nothing_and_warns_about_nothing() {
        let cs = ChangeSet {
            changes: vec![
                online(alter_nullability()),
                online(add_column(false, Some("0"))),
            ],
        };
        let e = Edition::classify("Developer Edition (64-bit)");
        assert!(online_not_supported(&cs, &e).is_empty());
        assert!(size_of_data_warnings(&cs, &e).is_empty());
    }

    #[test]
    fn a_limited_edition_refuses_online_and_warns_about_a_rewrite() {
        let cs = ChangeSet {
            changes: vec![
                online(alter_nullability()),
                online(add_column(false, Some("0"))),
            ],
        };
        let e = Edition::classify("Standard Edition (64-bit)");
        assert_eq!(online_not_supported(&cs, &e), vec!["dbo.customer"]);
        assert_eq!(size_of_data_warnings(&cs, &e).len(), 1);
    }

    /// `strategy:` sits on the table and so travels with every change to it,
    /// including the many the emitter writes no ONLINE clause for. Refusing on
    /// the annotation alone would leave a Standard-edition environment unable
    /// to plan an ordinary column addition against an annotated table.
    #[test]
    fn a_change_that_emits_no_online_clause_is_not_refused() {
        let e = Edition::classify("Standard Edition (64-bit)");
        for c in [
            add_column(true, None),
            pbps_model::PlannedChange::new(Change::AddCheck {
                table: "dbo.customer".parse().unwrap(),
                name: "ck_score".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "score > 0".into(),
                },
            }),
        ] {
            let cs = ChangeSet {
                changes: vec![online(c)],
            };
            assert!(
                online_not_supported(&cs, &e).is_empty(),
                "{:?}",
                cs.changes[0].change
            );
        }
    }

    /// A nullable column, or one with no default, is added as metadata on every
    /// edition; warning about it would train the reader to skip the warning.
    #[test]
    fn a_plain_column_addition_warns_about_nothing() {
        let e = Edition::classify("Standard Edition (64-bit)");
        for c in [add_column(true, Some("0")), add_column(false, None)] {
            let cs = ChangeSet { changes: vec![c] };
            assert!(size_of_data_warnings(&cs, &e).is_empty());
        }
    }
}
