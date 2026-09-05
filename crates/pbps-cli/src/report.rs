//! Turning results into text a person can read and act on.
//!
//! The principle here is that **every blocked situation comes with a command you
//! can copy and paste**. Non-interactive environments are never prompted
//! (constraint 6 in CLAUDE.md), so the error message itself has to be the
//! instructions — otherwise a user reading a CI log sees "this is ambiguous" with
//! no idea what to type.

use pbps_diff::Blocker;
use pbps_model::change::DeleteCause;
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
        Intent::RenameRole { from, to } => format!("role {to} renamed_from {from}"),
        Intent::DropRole { role, reason } => format!("drop role {role} (reason: {reason})"),
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

/// One blocker as a typed finding: a stable id, what happened, and the commands
/// that resolve it.
///
/// The remedy is the same text the human view prints — there is one description
/// of how to resolve an ambiguity, and a second one written for JSON would drift
/// from it the first time a command gained a flag.
pub fn blocker_finding(b: &Blocker) -> crate::output::Finding {
    let id = match b {
        Blocker::AmbiguousColumns { .. } => "identity.ambiguous-columns",
        Blocker::AmbiguousTables { .. } => "identity.ambiguous-tables",
        Blocker::DropColumnNeedsReason { .. } => "identity.drop-column-needs-reason",
        Blocker::DropTableNeedsReason { .. } => "identity.drop-table-needs-reason",
        Blocker::AmbiguousRoles { .. } => "identity.ambiguous-roles",
        Blocker::DropRoleNeedsReason { .. } => "identity.drop-role-needs-reason",
        Blocker::UnusedIntent { .. } => "identity.unused-intent",
    };
    let text = one_blocker(b);
    // The first line says what happened; the rest are the commands.
    let (message, remedy) = match text.split_once("\n\n") {
        Some((head, tail)) => (head.trim().to_owned(), tail.trim().to_owned()),
        None => (text.trim().to_owned(), String::new()),
    };
    let f = crate::output::Finding::error(id, message);
    if remedy.is_empty() {
        f
    } else {
        f.remedy(remedy)
    }
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
        Blocker::AmbiguousRoles {
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  role {} disappeared, {} is new\n\n",
                join(disappeared),
                join(appeared)
            );
            for from in disappeared {
                for to in appeared {
                    s.push_str(&format!(
                        "    if {from} was renamed to {to}:  pbps rename-role {from} {to}\n"
                    ));
                }
                s.push_str(&format!(
                    "    to drop {from}:                pbps drop-role {from} --reason \"<why>\"\n"
                ));
            }
            s
        }
        Blocker::DropRoleNeedsReason { role } => format!(
            "  role {role} disappeared from the declarations, but a drop must record why — \
             its members lose whatever it granted\n\n    pbps drop-role {role} --reason \"<why>\"\n"
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
/// How many distinct tables and how many modules a change set touches.
///
/// `Change::table()` returns the *object* name, which for a view, procedure,
/// function or trigger is the module's own name — so counting its distinct
/// values called a one-view plan "1 table". `module_name()` is what separates
/// them, and it exists precisely because they are different kinds of object
/// (ADR-0002).
pub fn touched(cs: &ChangeSet) -> (usize, usize) {
    let renames = renames(cs);
    let mut tables = std::collections::BTreeSet::new();
    let mut modules = std::collections::BTreeSet::new();
    for p in &cs.changes {
        match (p.change.module_name(), p.change.table()) {
            (Some(m), _) => modules.insert(m.to_string()),
            (None, Some(_)) => tables.insert(renames.resolve(p.change.subject())),
            // A role is neither; it is counted in its own line of the summary.
            (None, None) => false,
        };
    }
    (tables.len(), modules.len())
}

/// The new name of every object a change set renames, by its old name.
///
/// `RenameTable` and `RenameRole` answer [`Change::subject`] with the old
/// name, and every change that follows them in the same plan acts on the
/// new one: the differ speaks in the declared schema's names once the rename
/// is recorded. Counted as they come, the two spellings of one object made
/// a renamed role whose grants also change "2 role(s)". Every subject goes
/// through [`Renames::resolve`] before it is counted, so both ends land on
/// the same key.
struct Renames(std::collections::BTreeMap<String, String>);

impl Renames {
    fn resolve(&self, subject: String) -> String {
        self.0.get(&subject).cloned().unwrap_or(subject)
    }
}

fn renames(cs: &ChangeSet) -> Renames {
    Renames(
        cs.changes
            .iter()
            // Exhaustive, for the reason `Change::objects` gives: a change
            // added later that moves an object's name has to be named here,
            // or the summary goes back to counting that object twice.
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, to, .. } => Some((from.to_string(), to.to_string())),
                Change::RenameRole { from, to, .. } => {
                    Some((format!("role {from}"), format!("role {to}")))
                }
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. } => None,
            })
            .collect(),
    )
}

/// "3 table(s)", "2 module(s)", or both — never a count of one naming the
/// other.
pub fn objects(tables: usize, modules: usize, roles: usize) -> String {
    let mut parts = Vec::new();
    if tables > 0 || (modules == 0 && roles == 0) {
        parts.push(format!("{tables} table(s)"));
    }
    if modules > 0 {
        parts.push(format!("{modules} module(s)"));
    }
    if roles > 0 {
        parts.push(format!("{roles} role(s)"));
    }
    parts.join(" and ")
}

/// How many distinct roles a change set touches (ADR-0005).
pub fn touched_roles(cs: &ChangeSet) -> usize {
    let renames = renames(cs);
    cs.changes
        .iter()
        .filter(|p| p.change.table().is_none())
        .map(|p| renames.resolve(p.change.subject()))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

pub fn summary(cs: &ChangeSet) -> String {
    if cs.is_empty() {
        return String::new();
    }
    let (tables, modules) = touched(cs);
    let mut out = format!(
        "\n  {} change(s) across {}.\n",
        cs.changes.len(),
        objects(tables, modules, touched_roles(cs))
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
    // The advice names the *gated* classes only. A labelled-but-ungated one
    // (`grant-widen`, ADR-0005) is on every line it applies to above, and in
    // the summary; putting it in the `--allow` would advise a flag the gate
    // never asks for.
    let risks = cs.gated_risks();
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
    if cs.risks().contains(&RiskClass::GrantWiden) {
        out.push_str(
            "\n  This plan widens access (grant-widen). No flag gates it; the merge request is \
             where the grants are reviewed.\n",
        );
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
        let table = p.change.subject();
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
        // The analyzers' findings, under the change they are about, at the
        // severity the project chose (ADR-0008).
        for f in &p.findings {
            out.push_str(&format!("      {}: {} — {}\n", f.severity, f.id, f.message));
        }
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
    if !r.unexpressible.is_empty() {
        // First, because these are the ones no workflow can resolve: `pull`
        // cannot express them either, so the reader has to act by hand.
        out.push_str(&format!(
            "\n  {} difference(s) that cannot even be expressed as changes:\n",
            r.unexpressible.len()
        ));
        for e in &r.unexpressible {
            out.push_str(&format!("    {e}\n"));
        }
    }
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
            "\n  The plan does NOT converge: after applying it, the database still differs \
             from the declarations.\n",
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
            "\n  These differ only in how the engine spells them. Each costs one rebuilt \
             constraint per apply until the declaration is written in the stored form:\n",
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
        "  This is still a preview: it proves syntax and convergence, not edition \
         capabilities, and only `pbps plan --db` produces an applyable plan.\n",
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
        // Reference data (ADR-0004). The key identifies the row to a reviewer
        // who has no connection, so it leads every line.
        Change::InsertRow { key, row, .. } => {
            format!("+ row {key} ({} values)", row.0.len() + 1)
        }
        Change::UpdateRow { key, columns, .. } => {
            // Both ends, because the reviewer at the gate cannot look the old
            // value up — and "sets label" is not something anyone can approve.
            let cells: Vec<String> = columns
                .iter()
                .map(|(c, (from, to))| format!("{c} {from} -> {to}"))
                .collect();
            format!("~ row {key}: {}", cells.join(", "))
        }
        Change::DeleteRow { key, cause, .. } => match cause {
            DeleteCause::Undeclared => format!("- row {key} (not declared)"),
            DeleteCause::KeyChanged => format!("- row {key} (key changed)"),
        },
        Change::SetDataMode { to, .. } => match to {
            Some(m) => format!("~ reference data is now `{m}`"),
            None => "~ reference data is no longer declared".to_owned(),
        },
        // Roles (ADR-0005). The permissions are spelled out: "grant on
        // dbo.customer" tells a reviewer nothing about how much wider access
        // just got.
        Change::CreateRole { name, .. } => format!("+ create role {name}"),
        Change::DropRole { name, members, .. } if members.is_empty() => {
            format!("- drop role {name}")
        }
        Change::DropRole { name, members, .. } => format!(
            "- drop role {name}, removing {} member(s) first: {}",
            members.len(),
            members.join(", ")
        ),
        Change::RenameRole { from, to, .. } => format!("~ rename role {from} -> {to}"),
        Change::Grant {
            target,
            permissions,
            ..
        } => format!("+ grant {} on {target}", permissions_list(permissions)),
        Change::Revoke {
            target,
            permissions,
            ..
        } => format!("- revoke {} on {target}", permissions_list(permissions)),
    }
}

fn permissions_list(p: &std::collections::BTreeSet<pbps_model::Permission>) -> String {
    p.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
}

/// One argument, quoted so that pasting it passes the value through unchanged —
/// or `None` when no spelling can promise that.
///
/// # Why there is a `None`
///
/// This line is read in POSIX shells, PowerShell and `cmd`, and their quoting
/// rules do not overlap enough to cover everything:
///
/// - POSIX single quotes are literal, but **`cmd` does not treat `'` as quoting
///   at all**, so `&`, `|`, `<` and `>` stay live inside them. An earlier
///   version of this function used single quotes for exactly those characters
///   and claimed it failed safe in `cmd`; it does not — `cmd` would split the
///   command at the `&` and run the remainder.
/// - Double quotes are understood by all three for *splitting*, but POSIX
///   shells and PowerShell still expand `$` and a backtick inside them, and a
///   POSIX shell still reads `\\` as an escape inside them.
///
/// So there is no single string that is safe everywhere for a value containing
/// both families. Rather than pick a form that is wrong on one platform, this
/// returns `None` and the caller prints the path on a line of its own, where
/// nothing can execute it. A command that cannot be pasted blindly is a much
/// smaller problem than one that redirects or runs something when it is.
pub fn shell_arg(value: &str) -> Option<String> {
    // `~` is safe away from the front: it means home-directory expansion as the
    // first character of a word and nothing at all elsewhere, and every Windows
    // short path is full of it (`C:\Users\RUNNER~1\...`).
    //
    // `\` is deliberately *not* in this set, even though the same Windows paths
    // are full of it too. Bare is the one form a POSIX shell reads the
    // backslashes in: `C:\Users\RUNNER~1\plan.json` pasted unquoted arrives as
    // `C:UsersRUNNER~1plan.json`. It was added here to stop Windows paths being
    // quoted, which had the direction backwards — those are exactly the values
    // that need the quotes.
    //
    // `@` is the same shape as `~`, one shell further out: in PowerShell a token
    // *beginning* `@` in argument position is splatting, so `@args` would be
    // replaced by the current argument array rather than passed as the string
    // it is. Away from the front it means nothing, and `deploy@prod` is a
    // perfectly ordinary environment name — so the front is where it is
    // refused, and quoting it below makes PowerShell read it literally.
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:@+=~".contains(c);
    if !value.is_empty() && !value.starts_with(['~', '@', '-']) && value.chars().all(bare) {
        return Some(value.to_owned());
    }

    // A leading `-` is refused outright rather than quoted, because quoting
    // does not help: the shell strips the quotes and clap still receives an
    // argument beginning `-` and reads it as a flag — measured, not assumed
    // (`pbps explain --plan -plan.json` answers "unexpected argument '-p'").
    //
    // `--plan=-plan.json` *does* work, and emitting every option in that form
    // would carry these values. Not taken: it changes the shape of every
    // command this module advertises, and every test that reads one, to buy an
    // environment name or plan path beginning with a hyphen. The placeholder is
    // the established answer for a value that cannot be pasted, and this is one.
    if value.starts_with('-') {
        return None;
    }

    // Double quotes hold for a value a shell would only *split* — a space, most
    // often. They do not neutralize expansion (`$`, a backtick), a quote of the
    // same kind, a newline, or a trailing backslash, which would escape the
    // closing quote itself — and a Windows directory path ends with one more
    // often than not.
    //
    // A doubled backslash is refused for the same reason: inside POSIX double
    // quotes it collapses to a single one, which silently rewrites the leading
    // pair of a UNC path into a value that is still a valid path, just a
    // different one.
    let expands =
        value.contains(['$', '`', '"', '\n']) || value.ends_with('\\') || value.contains("\\\\");
    // Live in `cmd` whatever they are wrapped in, since `cmd` has no literal
    // quote character to wrap them in.
    //
    // `!` is in the list for a narrower reason: it is inert in a default `cmd`,
    // but under `setlocal enabledelayedexpansion` it expands *inside* double
    // quotes, so a path or environment name containing `!NAME!` would silently
    // become a different value on the one shell where this is hardest to
    // notice. Whether delayed expansion is on is not knowable from here, so the
    // safe reading is that it might be.
    let cmd_metacharacters = value.contains(['&', '|', '<', '>', '^', '%', '!']);
    if expands || cmd_metacharacters {
        return None;
    }
    Some(format!("\"{value}\""))
}

/// A `--env` argument for a copy-pastable remedy.
///
/// Environment names are YAML map keys, so `US West` is a perfectly valid one —
/// and interpolated verbatim it becomes two arguments. Where no cross-shell
/// spelling exists the caller gets `None` and should fall back to a placeholder;
/// a remedy that changes meaning when pasted is worse than one that has to be
/// completed by hand.
pub fn env_arg(name: &str) -> String {
    shell_arg(name).unwrap_or_else(|| "<environment>".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::change::PlannedChange;
    use pbps_model::{GrantTarget, Permission, Uid, UidKind};

    fn uid(kind: UidKind, seed: &str) -> Uid {
        Uid::derived(kind, seed, 0)
    }

    fn set(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }

    /// A `RenameRole` answers for its old name and the `Grant` that follows
    /// it in the same plan for the new one; counted as they come, one role
    /// read as two in the summary line and in the JSON count.
    #[test]
    fn a_renamed_role_whose_grants_also_change_is_one_role() {
        let cs = set(vec![
            Change::RenameRole {
                uid: uid(UidKind::Role, "a"),
                from: "a".into(),
                to: "b".into(),
            },
            Change::Grant {
                role: "b".into(),
                target: GrantTarget::Object("dbo.t".parse().unwrap()),
                permissions: [Permission::Select].into_iter().collect(),
            },
        ]);
        assert_eq!(touched_roles(&cs), 1);
        assert!(summary(&cs).contains("1 role(s)"), "{}", summary(&cs));
    }

    /// The same shape one namespace over: `RenameTable` answers for the old
    /// name, `AddColumn` on the renamed table for the new one.
    #[test]
    fn a_renamed_table_that_also_changes_is_one_table() {
        let cs = set(vec![
            Change::RenameTable {
                uid: uid(UidKind::Table, "dbo.old"),
                from: "dbo.old".parse().unwrap(),
                to: "dbo.new".parse().unwrap(),
            },
            Change::AddColumn {
                uid: uid(UidKind::Column, "dbo.new.extra"),
                table: "dbo.new".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
            },
        ]);
        assert_eq!(touched(&cs), (1, 0));
        assert!(summary(&cs).contains("1 table(s)"), "{}", summary(&cs));
    }

    /// Two roles that are not the two ends of one rename stay two: the
    /// collapse is by the rename's own pair, not by "a rename is present".
    #[test]
    fn a_renamed_role_and_an_unrelated_one_stay_two_roles() {
        let cs = set(vec![
            Change::RenameRole {
                uid: uid(UidKind::Role, "a"),
                from: "a".into(),
                to: "b".into(),
            },
            Change::Grant {
                role: "c".into(),
                target: GrantTarget::Schema("dbo".into()),
                permissions: [Permission::Select].into_iter().collect(),
            },
        ]);
        assert_eq!(touched_roles(&cs), 2);
        assert!(summary(&cs).contains("2 role(s)"), "{}", summary(&cs));
    }
}
