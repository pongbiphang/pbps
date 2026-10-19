//! Intent, the part that a human has to supply.
//!
//! # Why intent is a separate concept
//!
//! Almost every change can be derived automatically from what the declaration
//! files currently say. Three things cannot, because the information simply
//! is not in the files — it is only in the author's head (SPEC §6):
//!
//! - **Rename, or drop plus add**: when a table loses one column and gains
//!   another in the same revision, the two are structurally indistinguishable.
//! - **Why something was dropped**: the question an audit asks, and one no
//!   algorithm can answer.
//! - **Whether a DEFAULT partition's rows may move** into a new range
//!   partition (#1788): a move is a delete and an insert, and whether the
//!   environment tolerates that, and how many rows it may take, is in no
//!   file and no catalog.
//!
//! Intent does not go into [`crate::Schema`]. `Schema` must satisfy "two
//! semantically identical schemas are equal", and intent is one-shot: the same
//! desired state may have arrived via a rename or via a rebuild, so mixing it in
//! would break that equality (see constraint 1 in CLAUDE.md).

use crate::ColumnName;
use crate::Reason;
use crate::name::{ColumnRef, TableName};

/// One piece of human-supplied intent.
///
/// There are three sources, and they are exactly equivalent (SPEC §6): CLI
/// commands such as `pbps rename`, transient annotations in the declaration
/// files, and the interactive prompt. All of them end up in the identity file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    RenameTable {
        from: TableName,
        to: TableName,
    },
    RenameColumn {
        table: TableName,
        from: ColumnName,
        to: ColumnName,
    },
    /// Drop a table. `reason` is required for audit and cannot be omitted.
    DropTable {
        table: TableName,
        reason: Reason,
    },
    /// Drop a column.
    DropColumn {
        column: ColumnRef,
        reason: Reason,
    },
    /// A database role was renamed (ADR-0005). Membership follows the role
    /// through `ALTER ROLE ... WITH NAME`, which is why this is intent and not
    /// drop plus add.
    RenameRole {
        from: String,
        to: String,
    },
    /// Drop a role. "Why was this access removed" is an auditor's question.
    DropRole {
        role: String,
        reason: Reason,
    },
    /// Allow the DEFAULT partition's rows inside a new range partition's
    /// range to move into it, at most `max_rows` of them (#1788).
    MoveRows {
        table: TableName,
        max_rows: u64,
        reason: String,
    },
}

impl Intent {
    /// Whether this intent concerns the given table.
    pub fn concerns_table(&self, t: &TableName) -> bool {
        match self {
            Intent::RenameTable { from, to } => from == t || to == t,
            Intent::RenameColumn { table, .. }
            | Intent::DropTable { table, .. }
            | Intent::MoveRows { table, .. } => table == t,
            Intent::DropColumn { column, .. } => &column.table == t,
            Intent::RenameRole { .. } | Intent::DropRole { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    #[test]
    fn rename_concerns_both_old_and_new_table() {
        let i = Intent::RenameTable {
            from: t("dbo.customer"),
            to: t("dbo.client"),
        };
        assert!(i.concerns_table(&t("dbo.customer")));
        assert!(i.concerns_table(&t("dbo.client")));
        assert!(!i.concerns_table(&t("dbo.other")));
    }

    #[test]
    fn serialized_drop_intent_cannot_bypass_nonblank_reasons() {
        for (kind, target, name) in [
            ("drop_table", "table", "app.t"),
            ("drop_column", "column", "app.t.old"),
            ("drop_role", "role", "reader"),
        ] {
            for reason in ["", " ", "\t\n", "\u{3000}"] {
                let value = serde_json::json!({"intent": kind, target: name, "reason": reason});
                assert!(serde_json::from_value::<Intent>(value).is_err());
            }
            let valid = serde_json::json!({"intent": kind, target: name, "reason": "approved"});
            assert!(serde_json::from_value::<Intent>(valid).is_ok());
        }
    }

    #[test]
    fn round_trips_through_json() {
        let all = vec![
            Intent::RenameTable {
                from: t("dbo.a"),
                to: t("dbo.b"),
            },
            Intent::RenameColumn {
                table: t("dbo.a"),
                from: "x".into(),
                to: "y".into(),
            },
            Intent::DropTable {
                table: t("dbo.a"),
                reason: "REG-1".parse().unwrap(),
            },
            Intent::DropColumn {
                column: "dbo.a.x".parse().unwrap(),
                reason: "REG-2".parse().unwrap(),
            },
            Intent::RenameRole {
                from: "reader".into(),
                to: "app_reader".into(),
            },
            Intent::DropRole {
                role: "legacy".into(),
                reason: "REG-3".parse().unwrap(),
            },
            Intent::MoveRows {
                table: t("dbo.ev_2030"),
                max_rows: 5000,
                reason: "REG-4".into(),
            },
        ];
        let back: Vec<Intent> =
            serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
        assert_eq!(all, back);
    }
}
