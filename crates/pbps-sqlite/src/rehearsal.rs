//! The rehearsal's engine half (item 6 of #2950, DEC-2950.16): the scratch the
//! plan's statements run on, and the checks the statements do not make.
//!
//! The rehearsal itself is the engine-neutral `rehearse_in` of the command line:
//! it runs the plan as an apply runs it on a scratch, reads the scratch back and
//! compares it with the declarations, so it cannot disagree with `plan` about what
//! a difference is. What is SQLite's is here.
//!
//! **The scratch is a copy of the database's own pages** ([`scratch`]), made in
//! this process, in memory, on the linked library: nothing is started, nothing is
//! configured, and nothing is vouched for. The schema is exact (a function, a
//! collation or a virtual-table module the library lacks cannot be replayed from
//! text and is copied all the same) and the rows are there, so the engine itself
//! refuses what depends on them.
//!
//! **What the copy holds of the ledger** is whatever the file holds: the state
//! table's rows, the lock row, the environment identifier. Nothing reads them. A
//! rehearsal runs the plan's own statements and never `record`, `lock` or
//! `release`, which are an apply's, and the scratch is dropped when the rehearsal
//! ends. A lock taken on the copy would not be the file's (the sidecar is not
//! copied).
//!
//! **What a statement does not check** ([`resolution`]): a view that names what is
//! gone, a default calling a function that does not exist, a trigger that names
//! what is gone, a foreign key to a column that is no key. SQLite accepts them when they are declared and refuses
//! them when they are used, so a plan that leaves one behind runs every statement
//! and ends with a database that fails later. Asked before the plan and again
//! after it on the copy, so only what the plan broke is the plan's.

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, Record};
use pbps_model::escape::Escaped;
use pbps_model::{Change, ChangeSet, ColumnChange, TableChange};

use crate::lexis;
use crate::pragma::{self, foreign_key_list, table_list, table_xinfo};
use crate::state::{BUSY_WAIT, waiting};

/// The most a copy may take, in bytes: 512 MiB. A larger database is refused, and
/// the rehearsal is turned off by the operator, never skipped by the tool.
pub const SCRATCH_LIMIT: u64 = 512 * 1024 * 1024;

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// What every refusal ends with: the one way out, and what it records.
const WAY_OUT: &str = "Turn the rehearsal off with `--no-rehearse`; the plan is then recorded as \
                       not rehearsed.";

/// A copy of `target` for the rehearsal to run on, or the reason there is none.
///
/// `Refused` above `limit` (the size is read first, then enforced again on what
/// is copied: a database that grows past it during the copy is refused and the
/// partial copy dropped), and a busy database is the library's busy code with the
/// sentence that says what to do. `target` is not changed: no setting, no wait, no
/// transaction, no byte of the file.
pub fn scratch(target: &Connection, limit: u64) -> Result<Connection, DbError> {
    scratch_within(target, limit, BUSY_WAIT)
}

/// [`scratch`] with the wait given, so a test need not wait five seconds.
pub(crate) fn scratch_within(
    target: &Connection,
    limit: u64,
    wait: Duration,
) -> Result<Connection, DbError> {
    let library = target.version()?;
    let what = format!(
        "the rehearsal runs the plan on a copy of this database made in this process, in \
         memory, on SQLite {library}"
    );
    let size = waiting(target, wait, || target.size())
        .map_err(|error| explain_copy(error, &what, limit, wait))?;
    if size > limit {
        return Err(DbError::Refused(format!(
            "{what}, and the database is {} where a copy may be {} at most, so this plan \
             cannot be rehearsed. {WAY_OUT}",
            mib(size),
            mib(limit)
        )));
    }
    target
        .copy_to_memory(limit, wait)
        .map_err(|error| explain_copy(error, &what, limit, wait))
}

/// What a failed copy is told to the operator as: a refusal from the copy itself
/// is a database that grew past the limit after its size was read, and the busy
/// code is a database that was not free for the whole wait.
fn explain_copy(error: DbError, what: &str, limit: u64, wait: Duration) -> DbError {
    if matches!(error, DbError::Refused(_)) {
        return DbError::Refused(format!(
            "{what}, and the database grew past {} or changed its page size while it was \
             copied. Run again, or {WAY_OUT}",
            mib(limit)
        ));
    }
    let busy = error
        .server_error_code()
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| code & 0xff == 5);
    if busy {
        return error.context(format!(
            "{what}, and the database stayed busy for {} s while it was read or copied: another \
             connection is writing it, or this connection is inside a write transaction. Run \
             again when it is free. {WAY_OUT}",
            wait.as_secs_f64()
        ));
    }
    error
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Detail {
    /// The engine's own words for what failed.
    Failed(String),
    /// A count of rows.
    Rows(u64),
    /// The question could not be put: what it needs is itself broken (a key
    /// check that fails counts no row; a view whose columns cannot be read has
    /// no update to compile). It is a third answer, neither "resolves" nor
    /// "fails", and it is recorded because an answer that is absent reads as a
    /// clean one: once a plan mends what stood in the way, what was there all
    /// along is counted for the first time (#3021).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Unresolved {
    what: &'static str,
    /// As it is shown: the names the database has when it is asked.
    name: String,
    /// What it is a finding about, part by part, so that a finding from before
    /// a plan can be found again under the names the plan gave.
    about: Vec<About>,
    detail: Detail,
}

/// One part of what a finding is about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum About {
    /// A table or a view.
    Table(String),
    /// A column of a table.
    Column(String, String),
    /// Which write of a trigger's owner.
    Write(&'static str),
}

/// The names a plan changes, so that what failed before it under one name is
/// not counted as new under the other. Names are compared as SQLite compares
/// them, without ASCII case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renamed {
    /// A table's name before the plan and after it.
    tables: Vec<(String, String)>,
    /// A column's table as the plan leaves it named, then its name before and after.
    columns: Vec<(String, String, String)>,
}

impl Renamed {
    /// The table and column renames of `plan`.
    #[must_use]
    pub fn of(plan: &ChangeSet) -> Self {
        let mut renamed = Self::default();
        for planned in &plan.changes {
            // Every other change renames nothing the findings are named by.
            if let Change::Table(TableChange::RenameTable { from, to, .. }) = &planned.change {
                renamed.tables.push((from.name.clone(), to.name.clone()));
            }
            if let Change::Column(ColumnChange::RenameColumn {
                table, from, to, ..
            }) = &planned.change
            {
                renamed.columns.push((
                    table.name.clone(),
                    from.raw().to_owned(),
                    to.raw().to_owned(),
                ));
            }
        }
        renamed
    }

    fn table(&self, before: &str) -> String {
        self.tables
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(before))
            .map_or_else(|| before.to_owned(), |(_, to)| to.clone())
    }

    /// `about`, a part of a finding from before the plan, under the plan's names.
    fn after(&self, about: &About) -> About {
        match about {
            About::Table(table) => About::Table(self.table(table)),
            About::Column(table, column) => {
                let table = self.table(table);
                let column = self
                    .columns
                    .iter()
                    .find(|(of, from, _)| {
                        of.eq_ignore_ascii_case(&table) && from.eq_ignore_ascii_case(column)
                    })
                    .map_or_else(|| column.clone(), |(_, _, to)| to.clone());
                About::Column(table, column)
            }
            About::Write(kind) => About::Write(kind),
        }
    }
}

impl About {
    fn same(&self, other: &About) -> bool {
        match (self, other) {
            (About::Table(a), About::Table(b)) => a.eq_ignore_ascii_case(b),
            (About::Column(a, x), About::Column(b, y)) => {
                a.eq_ignore_ascii_case(b) && x.eq_ignore_ascii_case(y)
            }
            (About::Write(a), About::Write(b)) => a == b,
            _ => false,
        }
    }
}

/// What does not resolve in a database now. Compare two with [`Resolution::new_since`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution(Vec<Unresolved>);

/// Selects from every view, evaluates every column default, compiles a write to
/// everything that has a trigger and checks each table's foreign keys, on
/// `scratch`. Nothing is written. An engine refusal is a finding, not an
/// error: it is the thing being asked.
///
/// What there is to ask of is read with the catalog's pragmas as statements
/// ([`crate::pragma`]): a table named like a pragma's function would answer a
/// `SELECT` from it in its place, and an empty one would read as "no views, no
/// tables", so everything resolves.
pub fn resolution(scratch: &Connection) -> Result<Resolution, DbError> {
    let mut found = Vec::new();
    let mut broken_views: Vec<String> = Vec::new();
    for name in &listed(scratch, "view")? {
        let sql = format!("SELECT * FROM main.{} LIMIT 0", quote(name));
        if let Err(error) = scratch.query(&sql, &[]) {
            found.push(failed(
                "view",
                name.to_owned(),
                vec![About::Table(name.to_owned())],
                &error,
            ));
            broken_views.push(name.to_owned());
        }
    }
    // A virtual table's columns are not asked of (its module may be missing), nor
    // a shadow table's; a generated column has no default.
    for table in &listed(scratch, "table")? {
        for column in &visible_columns(scratch, table)? {
            let name = column.text(table_xinfo::NAME)?;
            let Some(default) = column.optional_text(table_xinfo::DEFAULT)? else {
                continue;
            };
            // A line feed before the closing parenthesis, so a default that ends
            // in a `--` comment does not swallow it.
            if let Err(error) = scratch.query(&format!("SELECT ({default}\n)"), &[]) {
                found.push(failed(
                    "default",
                    format!("{table}.{name}"),
                    vec![About::Column(table.clone(), name.to_owned())],
                    &error,
                ));
            }
        }
    }
    triggers(scratch, &broken_views, &mut found)?;
    // Asked table by table: a key the engine cannot check (a parent column that
    // is no key) fails the whole pragma, and one answer for the database would
    // hide a second such table behind the first.
    for table in &listed(scratch, "table")? {
        let keys = pragma::foreign_keys(scratch, table)?;
        if keys.is_empty() {
            continue;
        }
        // One answer for a table with a key: its check, and a write compiled
        // for it, on that condition alone. A check with no child row has
        // nothing to look a parent up for, so a parent that is gone goes unsaid
        // (measured) until the next insert fails. And the two are **one**
        // finding, the check's words first: asked apart, or the write only when
        // the check passed, what never compiled (a default the library cannot
        // evaluate) reads as new once a plan takes an orphaned or uncheckable
        // key away.
        let insert = format!("EXPLAIN INSERT INTO main.{} DEFAULT VALUES", quote(table));
        let written = scratch.query(&insert, &[]).err();
        let checked = scratch.query(
            &format!("PRAGMA main.foreign_key_check({})", quote(table)),
            &[],
        );
        if let Some(error) = checked.as_ref().err().or(written.as_ref()) {
            found.push(failed(
                "foreign keys",
                table.to_owned(),
                vec![About::Table(table.to_owned())],
                error,
            ));
        }
        match checked {
            // Not "no orphan row": none could be counted, for any key.
            Err(_) => found.push(Unresolved {
                what: "foreign key",
                name: table.to_owned(),
                about: vec![About::Table(table.to_owned())],
                detail: Detail::Unknown,
            }),
            Ok(rows) => {
                // Counted for each key and not for each parent: a plan that
                // replaces one key to a parent by another over other columns
                // can leave as many orphan rows as there were, and they are
                // not the ones there were.
                let mut counts: std::collections::BTreeMap<i64, u64> = Default::default();
                for row in &rows {
                    *counts.entry(row.integer(3)?).or_default() += 1;
                }
                for (id, rows) in counts {
                    let (mut parent, mut from, mut to) = (String::new(), Vec::new(), Vec::new());
                    for key in &keys {
                        if key.integer(foreign_key_list::ID)? == id {
                            key.text(foreign_key_list::TABLE)?.clone_into(&mut parent);
                            from.push(key.text(foreign_key_list::FROM)?.to_owned());
                            // Absent when the key names no parent column (the
                            // parent's primary key is meant).
                            to.extend(key.optional_text(foreign_key_list::TO)?.map(str::to_owned));
                        }
                    }
                    let mut about =
                        vec![About::Table(table.to_owned()), About::Table(parent.clone())];
                    about.extend(
                        from.iter()
                            .map(|c| About::Column(table.to_owned(), c.clone())),
                    );
                    about.extend(to.iter().map(|c| About::Column(parent.clone(), c.clone())));
                    found.push(Unresolved {
                        what: "foreign key",
                        name: format!(
                            "{table}({}) -> {parent}({})",
                            from.join(", "),
                            to.join(", ")
                        ),
                        about,
                        detail: Detail::Rows(rows),
                    });
                }
            }
        }
    }
    Ok(Resolution(found))
}

/// Compiles, and does not run, a write of each kind to every table and view
/// that has a trigger. The reader reads around triggers, and a trigger that names
/// what is gone is accepted by every statement and fails the application's next
/// write to its table. `EXPLAIN` compiles the trigger's program with the
/// statement (measured: `no such table: main.gone`) and writes nothing.
///
/// A view's columns are read only when the view itself resolved: the pragma of a
/// broken view fails, and that view is a finding already.
fn triggers(
    scratch: &Connection,
    broken_views: &[String],
    found: &mut Vec<Unresolved>,
) -> Result<(), DbError> {
    let owners = scratch.query(
        "SELECT DISTINCT tbl_name FROM sqlite_schema WHERE type = 'trigger' ORDER BY tbl_name",
        &[],
    )?;
    for owner in &owners {
        let owner = owner.text(0)?;
        let target = format!("main.{}", quote(owner));
        // A view takes a write only of a kind it has an `INSTEAD OF` trigger for
        // (measured: the others are refused as writes to a view, with no trigger
        // at fault), so only those are asked of it. A table takes every kind.
        let asked = view_writes(scratch, owner)?;
        let asks = |kind: &str| asked.as_ref().is_none_or(|kinds| kinds.contains(kind));
        let mut writes = Vec::new();
        if asks("insert") {
            writes.push((
                "insert",
                format!("EXPLAIN INSERT INTO {target} DEFAULT VALUES"),
            ));
        }
        if asks("delete") {
            writes.push(("delete", format!("EXPLAIN DELETE FROM {target}")));
        }
        // A trigger's owner is stored as the trigger wrote it, and a view's name
        // as the view did: one name to the engine, whatever the case.
        let broken = broken_views
            .iter()
            .any(|view| view.eq_ignore_ascii_case(owner));
        if asks("update") && broken {
            found.push(Unresolved {
                what: "trigger",
                name: format!("{owner} (update)"),
                about: vec![About::Table(owner.to_owned()), About::Write("update")],
                detail: Detail::Unknown,
            });
        }
        if asks("update") && !broken {
            // Every column is set to itself, so a trigger `OF` any column fires.
            let mut sets = Vec::new();
            for column in &visible_columns(scratch, owner)? {
                let name = quote(column.text(table_xinfo::NAME)?);
                sets.push(format!("{name} = {name}"));
            }
            if !sets.is_empty() {
                writes.push((
                    "update",
                    format!("EXPLAIN UPDATE {target} SET {}", sets.join(", ")),
                ));
            }
        }
        for (kind, sql) in writes {
            if let Err(error) = scratch.query(&sql, &[]) {
                found.push(failed(
                    "trigger",
                    format!("{owner} ({kind})"),
                    vec![About::Table(owner.to_owned()), About::Write(kind)],
                    &error,
                ));
            }
        }
    }
    Ok(())
}

/// The kinds of write the view `owner` has a trigger for, or `None` when
/// `owner` is a table (which takes every kind).
fn view_writes(
    scratch: &Connection,
    owner: &str,
) -> Result<Option<std::collections::BTreeSet<&'static str>>, DbError> {
    let name = [pbps_db::sqlite::Value::Text(owner.to_owned())];
    let views = scratch.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'view' AND name = ?1 COLLATE NOCASE",
        &name,
    )?;
    if views.is_empty() {
        return Ok(None);
    }
    let triggers = scratch.query(
        "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND tbl_name = ?1 COLLATE NOCASE",
        &name,
    )?;
    let mut kinds = std::collections::BTreeSet::new();
    for trigger in &triggers {
        match trigger_event(trigger.text(0)?) {
            Some(kind) => {
                kinds.insert(kind);
            }
            // A definition this cannot read: every kind is asked, as of a table,
            // and none is skipped on a guess.
            None => return Ok(None),
        }
    }
    Ok(Some(kinds))
}

/// The event of a `CREATE TRIGGER` definition: the first of `DELETE`, `INSERT`
/// and `UPDATE` as a bare word in its code. The three are reserved, so none can
/// be the trigger's unquoted name, and a quoted name or a comment is not code.
fn trigger_event(definition: &str) -> Option<&'static str> {
    for piece in lexis::pieces(definition) {
        let lexis::Piece::Code(code) = piece else {
            continue;
        };
        for word in code.split(|c| !crate::ident::identifier_continues(c)) {
            for (event, kind) in [
                ("DELETE", "delete"),
                ("INSERT", "insert"),
                ("UPDATE", "update"),
            ] {
                if word.eq_ignore_ascii_case(event) {
                    return Some(kind);
                }
            }
        }
    }
    None
}

fn failed(what: &'static str, name: String, about: Vec<About>, error: &DbError) -> Unresolved {
    Unresolved {
        what,
        name,
        about,
        detail: Detail::Failed(error.to_string()),
    }
}

/// The names `main` lists under one kind (`table` or `view`), in name order.
fn listed(scratch: &Connection, kind: &str) -> Result<Vec<String>, DbError> {
    let mut names = Vec::new();
    for row in pragma::by_name(pragma::tables(scratch)?, table_list::NAME)? {
        if row.text(table_list::KIND)? == kind {
            names.push(row.text(table_list::NAME)?.to_owned());
        }
    }
    Ok(names)
}

/// The columns of a table or view of `main` that a statement can name (not a
/// generated one, nor one hidden to a virtual table), in the table's order.
fn visible_columns(scratch: &Connection, of: &str) -> Result<Vec<Record>, DbError> {
    let mut kept = Vec::new();
    for column in pragma::sorted(pragma::columns(scratch, of)?, |column| {
        column.integer(table_xinfo::CID)
    })? {
        if column.integer(table_xinfo::HIDDEN)? == 0 {
            kept.push(column);
        }
    }
    Ok(kept)
}

fn quote(name: &str) -> String {
    ["\"", &name.replace('"', "\"\""), "\""].concat()
}

impl Resolution {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|item| item.detail == Detail::Unknown)
    }

    /// One line for each thing that does not resolve, as a note and not a
    /// refusal: asked of the resolution from before a plan, it is what was
    /// already broken, which [`Resolution::new_since`] does not charge the plan
    /// with.
    #[must_use]
    pub fn failing(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|item| match &item.detail {
                Detail::Failed(words) => Some(format!(
                    "{} `{}` does not resolve: {}",
                    item.what,
                    Escaped(&item.name),
                    Escaped(words)
                )),
                Detail::Rows(rows) => Some(format!(
                    "{rows} row(s) have no parent in `{}`",
                    Escaped(&item.name)
                )),
                // What stands in its way is a line of its own.
                Detail::Unknown => None,
            })
            .collect()
    }

    /// One line for each thing that fails now and could not be asked before the
    /// plan: not charged to the plan by [`Resolution::new_since`], and not known
    /// to be older than it either. A note beside [`Resolution::failing`].
    #[must_use]
    pub fn not_compared(&self, before: &Resolution, renamed: &Renamed) -> Vec<String> {
        let now = Resolution(
            self.0
                .iter()
                .filter(|item| before.unknown(item, renamed))
                .cloned()
                .collect(),
        );
        now.failing()
    }

    /// Whether `item`'s question could not be put when `self` was asked: an
    /// unknown of the same kind about the same things, or about the first of
    /// them (every key of a table at once).
    fn unknown(&self, item: &Unresolved, renamed: &Renamed) -> bool {
        self.0.iter().any(|b| {
            b.detail == Detail::Unknown
                && b.what == item.what
                && b.about.len() <= item.about.len()
                && b.about
                    .iter()
                    .zip(&item.about)
                    .all(|(was, now)| renamed.after(was).same(now))
        })
    }

    /// One line for each thing that fails now and did not fail before: what the
    /// plan broke. What failed before the plan and fails after it is not the
    /// plan's, **whatever the two sentences say**: a view over a function the
    /// library lacks fails before every plan, and the engine's words are not
    /// compared, because they quote names a plan may change and are the
    /// engine's to reword (#3016). So a thing already broken that a plan breaks
    /// in another way is not reported here; it did not work before the plan
    /// either, and [`Resolution::failing`] of the one from before names it.
    /// Foreign-key rows count only the ones added, for each key. And what could
    /// not be asked before the plan is not the plan's because it can be asked
    /// after it: absent, failing and unknown are three answers (#3021), and
    /// [`Resolution::not_compared`] names those.
    ///
    /// `renamed` is what the plan renames: a finding from before is looked for
    /// under the names the plan gave, so rows that had no parent in `p` are not
    /// new rows without a parent in `p2` when the plan only renamed the table.
    #[must_use]
    pub fn new_since(&self, before: &Resolution, renamed: &Renamed) -> Vec<String> {
        let mut lines = Vec::new();
        for item in &self.0 {
            if item.detail == Detail::Unknown || before.unknown(item, renamed) {
                continue;
            }
            let earlier = before.0.iter().find(|b| {
                b.what == item.what
                    && b.about.len() == item.about.len()
                    && b.about
                        .iter()
                        .zip(&item.about)
                        .all(|(was, now)| renamed.after(was).same(now))
            });
            match (&item.detail, earlier.map(|b| &b.detail)) {
                (Detail::Failed(_), Some(Detail::Failed(_))) => {}
                (Detail::Failed(now), _) => lines.push(match item.what {
                    "view" => format!(
                        "the view `{}` cannot be selected from after the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                    "default" => format!(
                        "the default of `{}` cannot be evaluated after the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                    "trigger" => format!(
                        "a trigger does not compile for the write `{}` after the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                    _ => format!(
                        "the foreign keys of `{}` cannot be checked, or a row cannot be inserted, after \
                         the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                }),
                (Detail::Unknown, _) => {}
                (Detail::Rows(now), Some(Detail::Rows(was))) if now <= was => {}
                (Detail::Rows(now), was) => {
                    let new = now
                        - was.map_or(0, |was| match was {
                            Detail::Rows(n) => *n,
                            Detail::Failed(_) | Detail::Unknown => 0,
                        });
                    lines.push(format!(
                        "{new} more row(s) have no parent in `{}` after the plan",
                        Escaped(&item.name)
                    ));
                }
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests;
