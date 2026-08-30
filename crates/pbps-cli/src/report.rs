//! Turning results into text a person can read and act on.
//!
//! The principle here is that **every blocked situation comes with a command you
//! can copy and paste**. Non-interactive environments are never prompted
//! (constraint 6 in CLAUDE.md), so the error message itself has to be the
//! instructions — otherwise a user reading a CI log sees "this is ambiguous" with
//! no idea what to type.

use pbps_diff::Blocker;
use pbps_model::{Change, ChangeSet, RiskClass};

pub fn blockers(list: &[Blocker]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} change(s) could not be decided automatically\n",
        list.len()
    ));
    for b in list {
        out.push('\n');
        out.push_str(&one_blocker(b));
    }
    out
}

fn one_blocker(b: &Blocker) -> String {
    match b {
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  {table}: {} disappeared, {} is new\n\n",
                disappeared.join(", "),
                appeared.join(", ")
            );
            for from in disappeared {
                for to in appeared {
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  pbps rename {table}.{from} {to}\n"
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop {table}.{from} --reason \"<why>\"\n"
                ));
            }
            s
        }
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  table {} disappeared, {} is new\n\n",
                join(disappeared),
                join(appeared)
            );
            for from in disappeared {
                for to in appeared {
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  pbps rename-table {from} {to}\n"
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop-table {from} --reason \"<why>\"\n"
                ));
            }
            s
        }
        Blocker::DropColumnNeedsReason { column } => format!(
            "  {column} disappeared from the declarations, but a drop must record why (an audit asks for it)\n\n    pbps drop {column} --reason \"<why>\"\n"
        ),
        Blocker::DropTableNeedsReason { table } => format!(
            "  table {table} disappeared from the declarations, but a drop must record why\n\n    pbps drop-table {table} --reason \"<why>\"\n"
        ),
        Blocker::UnusedIntent { intent } => {
            format!(
                "  this intent matches nothing in either the declarations or the identity file, likely a typo:\n    {intent:?}\n"
            )
        }
    }
}

fn join<T: std::fmt::Display>(v: &[T]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn plan(cs: &ChangeSet) -> String {
    if cs.is_empty() {
        return "No changes.\n".to_owned();
    }

    let mut out = String::new();
    let mut current = None;
    for p in &cs.changes {
        let table = p.change.table().to_string();
        if current.as_deref() != Some(table.as_str()) {
            out.push_str(&format!("\n  {table}\n"));
            current = Some(table);
        }
        let risks = if p.risks.is_empty() {
            String::new()
        } else {
            format!(
                "  [{}]",
                p.risks
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!("    {}{}\n", describe(&p.change), risks));
    }

    let risks = cs.risks();
    if !risks.is_empty() {
        out.push_str(&format!(
            "\n  This plan contains risky changes and needs approval to apply: --allow {}\n",
            risks
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
        if risks.contains(&RiskClass::Destructive) {
            out.push_str("  Some of them are destructive and will lose data.\n");
        }
    }
    out
}

fn describe(c: &Change) -> String {
    match c {
        Change::CreateTable { name, table, .. } => {
            format!("+ create table {name} ({} columns)", table.columns.len())
        }
        Change::DropTable { name, .. } => format!("- drop table {name}"),
        Change::RenameTable { from, to, .. } => format!("~ rename table {from} -> {to}"),
        Change::AddColumn { name, column, .. } => {
            format!("+ add column {name} {}", column.ty)
        }
        Change::DropColumn { column, .. } => format!("- drop column {}", column.name),
        Change::RenameColumn { from, to, .. } => format!("~ rename column {from} -> {to}"),
        Change::AlterColumnType {
            column, from, to, ..
        } => {
            format!("~ {} type {from} -> {to}", column.name)
        }
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => format!(
            "~ {} becomes {}",
            column.name,
            if *to_nullable { "nullable" } else { "NOT NULL" }
        ),
        Change::AlterColumnDefault { column, to, .. } => match to {
            Some(v) => format!("~ {} default -> {v}", column.name),
            None => format!("~ {} default removed", column.name),
        },
        Change::SetColumnDeprecated { column, reason, .. } => match reason {
            Some(r) => format!("~ {} marked deprecated: {r}", column.name),
            None => format!("~ {} no longer deprecated", column.name),
        },
        Change::SetPrimaryKey { to, .. } => match to {
            Some(pk) => format!("~ primary key -> ({})", pk.columns.join(", ")),
            None => "- drop primary key".to_owned(),
        },
        Change::AddUnique { name, .. } => format!("+ unique constraint {name}"),
        Change::DropUnique { name, .. } => format!("- unique constraint {name}"),
        Change::AddForeignKey { name, .. } => format!("+ foreign key {name}"),
        Change::DropForeignKey { name, .. } => format!("- foreign key {name}"),
        Change::AddCheck { name, .. } => format!("+ check constraint {name}"),
        Change::DropCheck { name, .. } => format!("- check constraint {name}"),
        Change::AddIndex { name, .. } => format!("+ index {name}"),
        Change::DropIndex { name, .. } => format!("- index {name}"),
    }
}
