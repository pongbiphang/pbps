//! Explicit decisions travel together; an incomplete answer is never persisted.

use anyhow::{Context as _, ensure};
use clap::{ArgAction, Args};
use pbps_model::{ColumnRef, Intent};

#[derive(Args)]
#[group(required = true, multiple = true)]
pub struct BatchArgs {
    /// Rename a column; repeat for each decision
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["COLUMN", "NEW"])]
    rename: Vec<String>,
    /// Rename a table
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["FROM", "TO"])]
    rename_table: Vec<String>,
    /// Rename a role
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["FROM", "TO"])]
    rename_role: Vec<String>,
    /// Drop a column with its own nonblank audit reason
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["COLUMN", "REASON"])]
    drop: Vec<String>,
    /// Drop a table with its own nonblank audit reason
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["TABLE", "REASON"])]
    drop_table: Vec<String>,
    /// Drop a role with its own nonblank audit reason
    #[arg(long, num_args = 2, action = ArgAction::Append, value_names = ["ROLE", "REASON"])]
    drop_role: Vec<String>,
}

/// The command boundary cannot ask resolution to record an empty answer.
pub struct IntentBatch(Vec<Intent>);

impl IntentBatch {
    pub fn into_vec(self) -> Vec<Intent> {
        self.0
    }
}

impl From<Intent> for IntentBatch {
    fn from(intent: Intent) -> Self {
        Self(vec![intent])
    }
}

impl BatchArgs {
    /// Parse both members before discovery. A per-value clap parser cannot
    /// distinguish a qualified name from the reason beside it (DEC-1791.1).
    pub fn parse(&self) -> anyhow::Result<IntentBatch> {
        let mut intents = Vec::new();
        for (flag, values) in [
            ("--rename", &self.rename),
            ("--rename-table", &self.rename_table),
            ("--rename-role", &self.rename_role),
            ("--drop", &self.drop),
            ("--drop-table", &self.drop_table),
            ("--drop-role", &self.drop_role),
        ] {
            let (pairs, remainder) = values.as_chunks::<2>();
            ensure!(remainder.is_empty(), "{flag} requires two values");
            for [first, second] in pairs {
                intents.push(parse_pair(flag, first, second).with_context(|| flag)?);
            }
        }
        ensure!(!intents.is_empty(), "supply at least one identity decision");
        Ok(IntentBatch(intents))
    }
}

fn parse_pair(flag: &str, first: &str, second: &str) -> anyhow::Result<Intent> {
    Ok(match flag {
        "--rename" => {
            let column: ColumnRef = first.parse()?;
            Intent::RenameColumn {
                table: column.table,
                from: column.name,
                to: second.to_owned().into(),
            }
        }
        "--rename-table" => Intent::RenameTable {
            from: first.parse()?,
            to: second.parse()?,
        },
        "--rename-role" => Intent::RenameRole {
            from: first.to_owned(),
            to: second.to_owned(),
        },
        "--drop" => Intent::DropColumn {
            column: first.parse()?,
            reason: second.parse()?,
        },
        "--drop-table" => Intent::DropTable {
            table: first.parse()?,
            reason: second.parse()?,
        },
        "--drop-role" => Intent::DropRole {
            role: first.to_owned(),
            reason: second.parse()?,
        },
        _ => unreachable!("only the six declared pair options reach this parser"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn a_batch_requires_complete_pairs_and_accepts_repeated_and_mixed_options() {
        for args in [
            vec!["pbps", "intent"],
            vec!["pbps", "intent", "--drop", "app.t.a"],
            vec![
                "pbps", "intent", "--drop", "app.t.a", "--rename", "app.t.b", "new",
            ],
        ] {
            assert!(crate::Cli::try_parse_from(args).is_err());
        }
        let cli = crate::Cli::try_parse_from([
            "pbps",
            "intent",
            "--drop",
            "app.t.a",
            "first reason",
            "--drop",
            "app.t.b",
            "second reason",
            "--rename-role",
            "reader",
            "auditor",
        ])
        .unwrap();
        let crate::Command::Intent(args) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.parse().unwrap().into_vec().len(), 3);
    }

    #[test]
    fn pairs_keep_names_and_meaningful_reasons_verbatim() {
        assert_eq!(
            parse_pair("--drop", "app.Customer.Legacy Name", " \tapproved\n ").unwrap(),
            Intent::DropColumn {
                column: "app.Customer.Legacy Name".parse().unwrap(),
                reason: " \tapproved\n ".parse().unwrap(),
            }
        );
        for flag in [
            "--rename",
            "--rename-table",
            "--rename-role",
            "--drop",
            "--drop-table",
            "--drop-role",
        ] {
            let first = if flag == "--rename" || flag == "--drop" {
                "app.t.old"
            } else {
                "app.t"
            };
            assert!(parse_pair(flag, first, "app.new").is_ok());
        }
    }

    #[test]
    fn pairs_refuse_bad_qualified_names_and_blank_reasons() {
        for flag in ["--rename", "--rename-table", "--drop", "--drop-table"] {
            assert!(parse_pair(flag, "unqualified", "new").is_err());
        }
        assert!(parse_pair("--rename-table", "app.old", "unqualified").is_err());
        for flag in ["--drop", "--drop-table", "--drop-role"] {
            let name = if flag == "--drop" {
                "app.t.old"
            } else {
                "app.t"
            };
            for reason in ["", " ", "\t\n", "\u{3000}"] {
                assert!(parse_pair(flag, name, reason).is_err());
            }
        }
    }
}
