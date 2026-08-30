//! Intent, the part that a human has to supply.
//!
//! # Why intent is a separate concept
//!
//! Almost every change can be derived automatically from what the declaration
//! files currently say. Only two things cannot, because the information simply
//! is not in the files — it is only in the author's head (SPEC §6):
//!
//! - **Rename, or drop plus add**: when a table loses one column and gains
//!   another in the same revision, the two are structurally indistinguishable.
//! - **Why something was dropped**: the question an audit asks, and one no
//!   algorithm can answer.
//!
//! Intent does not go into [`crate::Schema`]. `Schema` must satisfy "two
//! semantically identical schemas are equal", and intent is one-shot: the same
//! desired state may have arrived via a rename or via a rebuild, so mixing it in
//! would break that equality (see constraint 1 in CLAUDE.md).

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
        from: String,
        to: String,
    },
    /// Drop a table. `reason` is required for audit and cannot be omitted.
    DropTable {
        table: TableName,
        reason: String,
    },
    /// Drop a column.
    DropColumn {
        column: ColumnRef,
        reason: String,
    },
}

impl Intent {
    /// Whether this intent concerns the given table.
    pub fn concerns_table(&self, t: &TableName) -> bool {
        match self {
            Intent::RenameTable { from, to } => from == t || to == t,
            Intent::RenameColumn { table, .. } | Intent::DropTable { table, .. } => table == t,
            Intent::DropColumn { column, .. } => &column.table == t,
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
                reason: "REG-1".into(),
            },
            Intent::DropColumn {
                column: "dbo.a.x".parse().unwrap(),
                reason: "REG-2".into(),
            },
        ];
        let back: Vec<Intent> =
            serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
        assert_eq!(all, back);
    }
}
