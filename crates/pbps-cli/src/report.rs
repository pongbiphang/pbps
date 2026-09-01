//! Turning results into text a person can read and act on.
//!
//! The principle here is that **every blocked situation comes with a command you
//! can copy and paste**. Non-interactive environments are never prompted
//! (constraint 6 in CLAUDE.md), so the error message itself has to be the
//! instructions — otherwise a user reading a CI log sees "this is ambiguous" with
//! no idea what to type.

use pbps_diff::Blocker;
use pbps_model::{Change, ChangeSet, DriftReport, Intent, RiskClass};

/// One intent in the user's own vocabulary.
///
/// `Debug` would do at a pinch, but this string is shown to someone who has never
/// seen the `Intent` type — they wrote `renamed_from:` in a YAML file, or typed a
/// `pbps rename`, and that is what they should be shown.
pub fn intent(i: &Intent) -> String {
    match i {
        Intent::RenameTable { from, to } => format!("{to} renamed_from {from}"),
        Intent::RenameColumn { table, from, to } => format!("{table}.{to} renamed_from {from}"),
        Intent::DropTable { table, reason } => format!("drop table {table} (reason: {reason})"),
        Intent::DropColumn { column, reason } => format!("drop column {column} (reason: {reason})"),
    }
}

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
        Blocker::UnusedIntent { intent: i } => {
            format!(
                "  this intent matches nothing in either the declarations or the identity file, likely a typo:\n    {}\n",
                intent(i)
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

/// The headline: how much, where, and how risky.
///
/// Printed above the change list so that the first thing a reader sees is the
/// shape of the plan rather than its first line. A plan touching sixty tables
/// scrolls past; a plan touching one and dropping a column does not, and the two
/// must not look the same for the first screenful (SPEC §14.1).
///
/// Stable by construction: the counts come from the typed ChangeSet and the risk
/// order is [`RiskClass::ALL`], so two runs over the same plan produce the same
/// text and a diff of two summaries means the plans really differ.
pub fn summary(cs: &ChangeSet) -> String {
    if cs.is_empty() {
        return String::new();
    }
    let tables: std::collections::BTreeSet<String> = cs
        .changes
        .iter()
        .map(|p| p.change.table().to_string())
        .collect();
    let mut out = format!(
        "\n  {} change(s) across {} table(s).\n",
        cs.changes.len(),
        tables.len()
    );

    let risks = cs.risks();
    if risks.is_empty() {
        out.push_str("  No risk class applies; this plan needs no --allow.\n");
        return out;
    }
    for class in RiskClass::ALL {
        if !risks.contains(&class) {
            continue;
        }
        let n = cs
            .changes
            .iter()
            .filter(|p| p.risks.contains(&class))
            .count();
        out.push_str(&format!(
            "    {:<12} {n:>3} change(s) — {}\n",
            class.as_str(),
            class.why()
        ));
    }
    out
}

pub fn plan(cs: &ChangeSet) -> String {
    if cs.is_empty() {
        return "No changes.\n".to_owned();
    }

    let mut out = summary(cs);
    out.push_str(&changes(cs));
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

/// The change list alone, grouped by table.
///
/// Separate from [`plan`] because the `--allow` advice below it belongs to a
/// plan and to nothing else: a drift report describes what already happened, and
/// telling the reader which flag would approve it invites them to approve their
/// way past a schema someone changed by hand.
pub fn changes(cs: &ChangeSet) -> String {
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
    out
}

/// A drift report as prose.
///
/// The changes are phrased as "what the database has grown", because that is
/// what happened. Turning them round into "the plan would drop it" would smuggle
/// a remedy into a report whose job is to describe — and the remedy is a
/// judgement call with three legitimate answers (SPEC §8.3).
pub fn drift(r: &DriftReport) -> String {
    let mut out = format!(
        "Environment: {}\nBaseline:    entry #{} recorded {}\n",
        r.environment, r.baseline.entry_id, r.baseline.applied_at
    );

    if !r.unmanaged.is_empty() {
        out.push_str(&format!(
            "Unmanaged:   {} table(s) left alone: {}\n",
            r.unmanaged.len(),
            join(&r.unmanaged)
        ));
    }

    if !r.has_drift() {
        out.push_str("\nNo drift: the database matches its recorded state.\n");
        return out;
    }

    out.push_str("\nDRIFT: the database no longer matches its recorded state.\n");
    if !r.changes.is_empty() {
        out.push_str("\n  Differences found (recorded state -> database as it is now):\n");
        // The plan's own vocabulary, indented, minus its `--allow` advice: a
        // reader who has read one plan can read this without learning a second
        // vocabulary, but nothing here is waiting to be approved.
        for line in changes(&r.changes).lines() {
            if line.trim().is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("  {line}\n"));
            }
        }
    } else {
        // The checksums differ but the differ produced nothing: something the
        // model does not carry has changed. Saying so is far better than an
        // empty list that reads like "nothing, really".
        out.push_str(
            "\n  The state fingerprints differ, but no difference could be expressed as a\n  \
             change. Something outside what pbps models has moved; compare the recorded\n  \
             state_json by hand.\n",
        );
    }
    out.push_str(
        "\n  Three ways out (SPEC 8.3): fold it into the declarations (`pbps pull`), put the\n  \
         database back (`pbps plan --db` then `apply`), or accept it (`pbps baseline --reason`).\n",
    );
    out
}

/// What a dev-database rehearsal found (SPEC §9.3).
pub fn rehearsal(r: &crate::dev::Rehearsal) -> String {
    let mut out = format!(
        "\nDev rehearsal: built the baseline in {} statement(s), applied {} more.\n",
        r.built, r.applied
    );
    if !r.structural.is_empty() {
        out.push_str(
            "\n  The plan does NOT converge: after applying it, the database still differs\n               from the declarations.\n",
        );
        for d in &r.structural {
            out.push_str(&format!("    {d}\n"));
        }
    }
    if !r.spelling.is_empty() {
        // The one thing no offline normalization can produce: what the engine
        // actually stored. Rewriting the declaration in that form is what makes
        // the difference stop being reported after every apply.
        out.push_str(
            "\n  These differ only in how the engine spells them. Each costs one rebuilt\n               constraint per apply until the declaration is written in the stored form:\n",
        );
        for d in &r.spelling {
            out.push_str(&format!("    {d}\n"));
        }
    }
    if r.structural.is_empty() && r.spelling.is_empty() {
        out.push_str("  The declarations compile and the plan converges on them.\n");
    }
    // A container runs Developer edition — the Enterprise feature set — while
    // production may be Standard. Saying so keeps a green rehearsal from
    // reading as a promise it cannot make (ADR-0003 decision 3).
    out.push_str(
        "  This is still a preview: it proves syntax and convergence, not edition\n           capabilities, and only `pbps plan --db` produces an applyable plan.\n",
    );
    out
}

pub fn describe(c: &Change) -> String {
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
        Change::CreateModule { module, .. } => format!("+ create {}", module.kind),
        // "restate", not "alter": the whole definition is sent, which is what
        // `CREATE OR ALTER` does and what the reviewer is approving.
        Change::AlterModule { module, .. } => format!("~ restate {}", module.kind),
        Change::DropModule { kind, .. } => format!("- drop {kind}"),
    }
}
