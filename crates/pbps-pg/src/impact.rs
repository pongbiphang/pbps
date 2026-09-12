//! What a rename breaks (SPEC §7.4).
//!
//! # The catalog here lists what does *not* break
//!
//! The SQL Server module of the same name reads
//! `sys.sql_expression_dependencies` and reports what it finds, because that
//! engine stores module *text*: `sp_rename` does not touch it, so every
//! referrer the catalog names is a referrer about to break.
//!
//! **Measured on 18.6, this engine is the exact inverse.** Dependencies are
//! resolved at creation time and stored parsed, so `RENAME COLUMN` rewrites
//! them — and `pg_depend` holds an edge for precisely the objects that survive:
//!
//! ```text
//! CREATE VIEW v AS SELECT id, email FROM customer;
//! CREATE FUNCTION f_atomic() RETURNS text LANGUAGE sql
//!   BEGIN ATOMIC SELECT email FROM customer LIMIT 1; END;
//! CREATE FUNCTION f_plpgsql() RETURNS text LANGUAGE plpgsql
//!   AS $$ BEGIN RETURN (SELECT email FROM customer LIMIT 1); END $$;
//!
//! ALTER TABLE customer RENAME COLUMN email TO contact_email;
//!
//! -- edges from f_atomic  to customer: 1
//! -- edges from f_plpgsql to customer: 0
//! ```
//!
//! After the rename the view reads `SELECT id, contact_email AS email`, the
//! `BEGIN ATOMIC` body reads `customer.contact_email AS email`, the check
//! constraint reads `total >= 0`, the generated column reads `upper(ident)` and
//! the row-level policy reads `contact_email <> 'blocked'`. The `plpgsql`
//! function still reads `email`, and fails the next time anybody calls it:
//! `column "email" does not exist`.
//!
//! So a report that queried `pg_depend` and stopped would list the objects that
//! are fine and none of the objects that are broken — which is worse than no
//! report, because it looks like one. What breaks here is invisible to the
//! dependency graph by construction, and the only way to find it is to read the
//! bodies the graph does not parse.
//!
//! # Three lists, because there are three answers
//!
//! - **`advisory`** — this will break, and the engine will not stop the rename.
//!   Routines whose body is text, and names that embed the old column name.
//! - **`carried`** — this follows the rename and keeps working. Reported rather
//!   than left out: it is half of "what does this rename affect", it is the
//!   half this engine is *better* at, and an operator who cannot see it has to
//!   assume the worst about every view in the database. One item of it is worth
//!   saying out loud — measured, a view keeps its **old output column name** as
//!   an alias, so the view's own consumers see no change at all.
//! - **`blocking`** — the engine refuses the statement outright. Measured,
//!   nothing blocks a *rename* on this engine: a column a view depends on
//!   renames happily, where `DROP COLUMN` on the same column is refused with
//!   `cannot drop column ident of table t because other objects depend on it`.
//!   The list is kept because the abstraction has it and because the drop side
//!   is real; this module does not fill it, and says so rather than leaving a
//!   reader to conclude the query failed.
//!
//! # What this module does not answer
//!
//! **A module rename is a drop plus a create** (ADR-0002), and what the catalog
//! holds against a module about to be dropped is [`crate::modules`]'s question,
//! answered there in full — every reverse `pg_depend` edge, the classes with no
//! rule, the cycle, and what a rebuild cannot carry. Asking it a second time
//! here would be a second implementation of one question, and the two would
//! disagree the first time either was fixed.
//!
//! # What no query can see
//!
//! Applications, reports and downstream ELT are invisible to any of this, and
//! so is a routine whose body names a *different* table's column of the same
//! spelling — the body is text, and text is all there is to match. The report
//! says so rather than implying the list is exact; §7.4 puts a checklist in
//! front of a human for exactly that reason.

use std::collections::BTreeMap;

use pbps_db::{Conn, DbError, Row};
use pbps_dialect::DialectError;
use pbps_model::TableName;

use crate::emit::qualified;

// The target, the referrer, the report and its error are `pbps-db`'s: one
// shape, filled by each engine's own catalog queries (DECISIONS 417). Which
// changes in a plan are rename targets stays this engine's question —
// `rename_targets` below — and `RenameTarget::Module` is never one of them
// here: the module documentation says whose question a dropped module is.
pub use crate::drop_impact::drop_blockers;
pub use pbps_db::impact::{ImpactError, ImpactReport, Referrer, RenameTarget};

/// Every rename in a plan, as the objects they are renamed **from** — the
/// name the catalog still knows them by. Taking the new name would query
/// something that does not exist yet and report no impact at all, which is
/// the most dangerous possible answer.
///
/// **Both halves of a column's name** have to be taken back, not just the
/// column's own. A `RenameColumn` carries the *declared*, post-rename table
/// (`pbps_diff::schema_diff::order_key` class 3 says why: the statement it
/// becomes names the table, and it runs after the table rename), so a plan
/// that renames `app.client` to `app.customer` and its `email` to
/// `contact_email` describes the column as `app.customer.email` — a table
/// the catalog will not have until a statement this report runs *before*
/// has executed. Read literally that is `ImpactError::Name` on a plan the
/// engine would accept, which is the refusal
/// `preflight::AsStored` exists to prevent one rank further on
/// (DECISIONS 407).
pub fn rename_targets(changes: &pbps_model::ChangeSet) -> Vec<RenameTarget> {
    use pbps_model::Change;
    // The plan's name for a table, to the catalog's. Built first and over
    // the whole plan, because the ordering that puts a table rename before
    // its column renames is `order_key`'s and not this list's to assume.
    let mut stored: BTreeMap<&TableName, &TableName> = BTreeMap::new();
    for p in &changes.changes {
        if let Change::RenameTable { from, to, .. } = &p.change {
            stored.insert(to, from);
        }
    }
    changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::RenameTable { from, .. } => Some(RenameTarget::Table(from.clone())),
            Change::RenameColumn { table, from, .. } => {
                let table = stored.get(table).map_or(table, |t| *t);
                Some(RenameTarget::Column(table.column(from)))
            }
            // A module rename reaches the plan as a drop plus a create, and
            // what points at the old name is `crate::modules`' question.
            // Exhaustive rather than `_`: a change added later that moves a
            // name has to be considered here, and a catch-all would let it
            // through in silence.
            Change::DropModule { .. }
            | Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
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
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            // A row's identity is its key, and a changed key is a delete
            // plus an insert rather than a rename.
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
        })
        .collect()
}

/// Every routine in this database whose body the engine keeps as **text**,
/// with the body, for the ones that mention a given string anywhere.
///
/// `prosqlbody IS NULL` is the whole test, and it is the engine's own record of
/// which bodies it parsed — measured, a `BEGIN ATOMIC` SQL function has one and
/// a `plpgsql` function and a plain SQL function do not. A language list would
/// have been the obvious spelling and would be wrong twice: `sql` appears on
/// both sides of the line, and a procedural language installed later is not on
/// any list written today.
///
/// `strpos` and not a regular expression: an identifier is user text and would
/// have to be escaped into a pattern, and getting that wrong is a report that
/// silently matches nothing. This is the cheap filter; [`mentions`] is the
/// exact one, and it runs here where the rules are testable without a server.
///
/// The case-insensitive half of the filter folds under `COLLATE "C"`
/// explicitly, on both sides, rather than trusting the unqualified `lower()`
/// this query used to call (DECISIONS 449). `prosrc` is plain `text`, so an
/// unqualified `lower()` runs under the *database's* default collation, and
/// that is measurably the wrong fold: on a database created with Turkish
/// casing rules, `lower('I')` is dotless `ı` while `lower('i')` stays `i`, so
/// `strpos(lower('SELECT I FROM t'), lower('i'))` is `0` where the ASCII fold
/// gives `8` — a routine naming the target with a bare, differently-cased
/// letter would have been excluded here before [`mentions`] ever saw it,
/// which is the exact silence #260 exists to remove. `COLLATE "C"` folds
/// ASCII only and is locale-independent, which is *exactly* the engine's own
/// identifier fold (DECISIONS 230, 313) rather than an approximation of it,
/// so this half of the filter is now a strict superset of what [`mentions`]
/// accepts by construction, not by hope: an ASCII, per-byte fold cannot turn
/// a real occurrence of `$1` into a string that no longer contains it,
/// whatever `$1` or the body hold. That is also why there is only one test
/// here and not two — an exact quoted-spelling test beside this one was kept
/// in an earlier revision as a hedge against the unqualified `lower()`'s
/// locale dependence, and there is nothing left for it to hedge against once
/// the fold itself is exact: an exact match is a special case of a fold that
/// cannot lose it.
///
/// **Every schema the tool did not rule out**, and not only the managed ones:
/// an undeclared `plpgsql` function that reads a managed table is exactly the
/// referrer nobody will notice, and it breaks the same way.
///
/// Extension-owned routines are left out. They are somebody else's objects,
/// installed and upgraded by somebody else's script, and a rename in a schema
/// this tool manages cannot be the reason one of them is listed
/// (ADR-0009, DECISIONS 305).
const TEXT_BODIED_ROUTINES: &str = "\
SELECT n.nspname AS schema_name,
       p.proname AS routine_name,
       pg_catalog.pg_get_function_identity_arguments(p.oid) AS args,
       CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END AS kind,
       l.lanname AS language,
       p.prosrc AS body
  FROM pg_catalog.pg_proc p
  JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
  JOIN pg_catalog.pg_language l ON l.oid = p.prolang
 WHERE p.prosqlbody IS NULL
   AND l.lanname NOT IN ('internal', 'c')
   AND n.nspname NOT IN ('pg_catalog', 'information_schema')
   AND n.nspname NOT LIKE 'pg\\_%'
   AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                    WHERE d.classid = 'pg_catalog.pg_proc'::regclass
                      AND d.objid = p.oid AND d.deptype = 'e')
   AND pg_catalog.strpos(pg_catalog.lower(p.prosrc COLLATE \"C\"), pg_catalog.lower($1 COLLATE \"C\")) > 0
 ORDER BY n.nspname, p.proname";

/// Everything the catalog holds an edge to, described in the engine's own
/// words — the objects a rename is carried into.
///
/// `pg_describe_object` rather than a description assembled here: it is the
/// same wording the engine puts in the `DETAIL` of a refusal, so a reviewer
/// reading the report and an operator reading a failure see one description.
///
/// `refobjsubid` is how a column target narrows it. `0` is a reference to the
/// relation as a whole, which a column rename does reach — a view written
/// `SELECT *` is expanded at creation and holds one edge per column, but a
/// foreign key or a trigger on the table as a whole holds a `0`, and dropping
/// those would report a column rename as reaching nothing.
///
/// The internal edges (`deptype = 'i'`) are the object's own parts — a view's
/// `_RETURN` rule is the view — and listing them would report the object as its
/// own dependant.
const CARRIED: &str = "\
SELECT DISTINCT pg_catalog.pg_describe_object(d.classid, d.objid, d.objsubid) AS described
  FROM pg_catalog.pg_depend d
 WHERE d.refclassid = 'pg_catalog.pg_class'::regclass
   AND d.refobjid = pg_catalog.to_regclass($1)
   AND d.deptype <> 'i'
   AND ($2 = 0 OR d.refobjsubid IN (0, $2))
 ORDER BY 1";

/// Indexes and constraints on the table whose **name** embeds the old column
/// name. The objects keep working — measured, `ix_customer_email` is still
/// `ix_customer_email` and its definition reads the new column — and only the
/// naming goes stale.
const NAMED_OBJECTS: &str = "\
SELECT 'index' AS kind, ic.relname AS name
  FROM pg_catalog.pg_index i
  JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
 WHERE i.indrelid = pg_catalog.to_regclass($1)
UNION ALL
SELECT 'constraint', con.conname
  FROM pg_catalog.pg_constraint con
 WHERE con.conrelid = pg_catalog.to_regclass($1)
 ORDER BY 1, 2";

/// The column's `attnum`, which is what `pg_depend` records — **not** its
/// position: a dropped column keeps its slot, so the two part company the first
/// time anybody drops one (ADR-0012 §6).
const ATTNUM: &str = "\
SELECT a.attnum::int4 AS attnum
  FROM pg_catalog.pg_attribute a
 WHERE a.attrelid = pg_catalog.to_regclass($1)
   AND a.attname = $2 AND NOT a.attisdropped";

/// Queries every impact of one rename.
pub async fn rename_impact(
    conn: &mut Conn,
    target: &RenameTarget,
) -> Result<ImpactReport, ImpactError> {
    // The quoted, qualified spelling, because every query here hands the name
    // to `to_regclass`, which *parses* its argument. `to_regclass` and not a
    // `::regclass` cast: the cast raises for a name that is not there and the
    // function answers NULL, and a raise in the middle of an impact report is
    // an error where the honest answer is an empty list (DECISIONS 336's
    // shape).
    // Never built by `rename_targets`, and refused rather than answered if
    // handed in from elsewhere: a module has no `attnum`, so the queries
    // below would ask about "every column" of a relation `to_regclass` does
    // not know and come back empty — the one answer this module exists not
    // to give by mistake.
    if let RenameTarget::Module(module) = target {
        return Err(ImpactError::Name(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "module {module} is not a rename target on this engine: what depends on a \
                 module about to be dropped is `modules::dependents`'s question"
            ),
        }));
    }
    let relation = qualified(target.table())?;
    let mut report = ImpactReport {
        target: target.to_string(),
        ..Default::default()
    };

    // The column's attnum, or 0 for a table target — the value `CARRIED` reads
    // as "every column".
    let attnum: i32 = match target.column() {
        None => 0,
        Some(name) => {
            let rows = conn
                .query_with(ATTNUM, &[relation.as_str().into(), name.into()])
                .await?;
            match rows.first() {
                Some(row) => row.try_get::<i32>("attnum")?.unwrap_or(0),
                // The column is not in the catalog. That is not "nothing
                // depends on it": it is a question that could not be asked, and
                // the caller has to hear it as one.
                None => {
                    return Err(ImpactError::Name(DialectError::Invalid {
                        dialect: crate::types::DIALECT,
                        message: format!(
                            "{target} is not a column this database has, so what a rename of it \
                             would affect cannot be read"
                        ),
                    }));
                }
            }
        }
    };

    // What breaks: a body the engine never parsed, which still spells the old
    // name.
    let needle = target.column().unwrap_or(&target.table().name);
    for row in conn
        .query_with(TEXT_BODIED_ROUTINES, &[needle.into()])
        .await?
    {
        let body = text(&row, "body")?;
        if !mentions(&body, needle) {
            continue;
        }
        let args = text(&row, "args")?;
        report.advisory.push(Referrer {
            kind: text(&row, "kind")?,
            name: format!(
                "{}.{}({args})",
                text(&row, "schema_name")?,
                text(&row, "routine_name")?
            ),
            detail: Some(format!(
                "its {} body is text this engine never parsed, so the rename does not reach it \
                 and the failure lands the next time it runs",
                text(&row, "language")?
            )),
        });
    }

    // What goes stale in name only.
    if let Some(column) = target.column() {
        for row in conn
            .query_with(NAMED_OBJECTS, &[relation.as_str().into()])
            .await?
        {
            let name = text(&row, "name")?;
            if !name.contains(column) {
                continue;
            }
            report.advisory.push(Referrer {
                kind: text(&row, "kind")?,
                name,
                detail: Some("the name embeds the old column name".to_owned()),
            });
        }
    }

    // And what the rename is carried into.
    for row in conn
        .query_with(CARRIED, &[relation.as_str().into(), attnum.into()])
        .await?
    {
        report.carried.push(Referrer {
            kind: "carried".to_owned(),
            name: text(&row, "described")?,
            detail: None,
        });
    }
    Ok(report)
}

/// The lines a caller prints under the report, and the reason they are here
/// rather than in the caller: what an empty list *means* is this module's to
/// say.
pub fn notes(report: &ImpactReport) -> Vec<String> {
    let mut out = Vec::new();
    if !report.carried.is_empty() {
        out.push(
            "Those are carried into the new name by the engine and keep working. A view also \
             keeps its old output column name as an alias, so the view's own consumers see no \
             change."
                .to_owned(),
        );
    }
    if !report.advisory.is_empty() {
        out.push(
            "A routine body this engine stores as text is matched by name, so one of those may \
             be naming a different object that happens to share the spelling."
                .to_owned(),
        );
    }
    out.push(
        "Nothing outside the database is visible here: applications and downstream consumers \
         need a human's checklist."
            .to_owned(),
    );
    out
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    Ok(row.try_get::<&str>(column)?.unwrap_or_default().to_owned())
}

/// Whether a stored body names this object.
///
/// Bounded by the characters around it, so that `amount` does not match
/// `amount_paid` — a prefix match would flag every routine that mentions a
/// similarly named column and train the operator to skip the report. The
/// quoted spelling counts too: `"amount"` is the same name written the other
/// way, and a body that quotes it breaks just the same.
///
/// A dollar sign is an identifier character here and `@` and `#` are not, which
/// is where this parts company with the SQL Server rule it is otherwise the
/// same as.
///
/// # Unquoted folds, quoted does not (DECISIONS 449)
///
/// The engine downcases an *unquoted* identifier before it is stored
/// (DECISIONS 230, 313), so a bare `EMAIL` in a routine body names the same
/// column as `email`. A *quoted* identifier is stored exactly as written, so
/// `"EMAIL"` and `"email"` are two different columns, and folding the quoted
/// form too would report a routine that does not actually break — the trap
/// this scan exists to avoid, not the one it should fall into.
///
/// This is why the fold only ever runs when `name` itself has no **ASCII**
/// uppercase letter left in it. A name that does can only be in the catalog
/// because it was created quoted: no unquoted spelling folds to it, so a bare
/// mention that happens to share its letters case-insensitively is always
/// naming a *different*, lower-spelled column, not this one. The test is
/// ASCII-only on purpose and not "any uppercase letter": measured (DECISIONS
/// 230), an unquoted `CREATE TABLE AÄ` still makes the relation `aÄ`, so a
/// non-ASCII uppercase letter does not block the fold — only an ASCII one,
/// which the engine's own downcasing would always have removed, does.
///
/// The fold is ASCII-only, [`str::to_ascii_lowercase`] rather than
/// [`str::to_lowercase`], for the same reason `RoutineArg` and the reference
/// scan both are (DECISIONS 230, 313): the engine downcases an unquoted
/// identifier byte by byte and leaves the high bit alone, where Rust's
/// Unicode fold turns one character into a name the engine never produces.
/// `to_ascii_lowercase` also never changes a string's byte length or its char
/// boundaries, which is what keeps the stepping rule below correct on the
/// folded body without change.
///
/// # Why the step is a character and not a byte
///
/// A rejected match is stepped over by the width of the name's **first
/// character**, because `body[from..]` is a string slice and Rust refuses one
/// that starts inside a character. Stepping by one byte is the obvious way to
/// write this and it panics: scanning `xä` for `ä` finds it at byte 1, rejects
/// it because `x` is an identifier byte before it, and a one-byte step lands on
/// byte 2 — the middle of the two bytes `ä` occupies. Identifiers here are not
/// ASCII-only, deliberately: `is_ident_byte` counts every non-ASCII byte as
/// part of a name, and the quoting rule admits any character a `"…"` can hold
/// (DECISIONS 408).
fn mentions(body: &str, name: &str) -> bool {
    // No name is no scan. `find("")` matches at every position, so an empty
    // needle would report that every body in the database mentions it — and
    // with the step below it would still terminate, which makes the wrong
    // answer the quiet one.
    let Some(first) = name.chars().next() else {
        return false;
    };
    if body.contains(&format!("\"{name}\"")) {
        return true;
    }
    if bounded_match(body, name, first, false) {
        return true;
    }
    // See "Unquoted folds, quoted does not" above: a name with an ASCII
    // uppercase letter could only exist quoted, so no unquoted spelling ever
    // names it and the fold below must not run.
    if name != name.to_ascii_lowercase() {
        return false;
    }
    bounded_match(&body.to_ascii_lowercase(), name, first, true)
}

/// One scan for a stand-alone occurrence of `name` in `body`, shared by the
/// exact-case pass and the case-folded one.
///
/// `reject_quoted` is what keeps them from stepping on each other: a match
/// immediately bounded by `"` on both sides is a quoted identifier, whose
/// exactness is the earlier `"{name}"` check's job alone. The exact-case pass
/// leaves it in (a same-case quoted match is also caught here, harmlessly,
/// same as before this function existed); the case-folded pass must reject
/// it, or `"EMAIL"` would count as the same column as `email`.
fn bounded_match(body: &str, name: &str, first: char, reject_quoted: bool) -> bool {
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(offset) = body[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let before = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before && after {
            let quoted =
                start > 0 && bytes[start - 1] == b'"' && end < bytes.len() && bytes[end] == b'"';
            if !(reject_quoted && quoted) {
                return true;
            }
        }
        from = start + first.len_utf8();
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || !b.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Change, ChangeSet, PlannedChange};

    fn tname(s: &str) -> TableName {
        s.parse().expect("a table name")
    }

    /// The catalog knows the object by its **old** name, so that is the one to
    /// query with. Taking `to` would query a name that does not exist yet and
    /// report no impact at all — the most dangerous possible answer.
    ///
    /// **Both halves of the column's name**, which is the case this fixture
    /// carries: the plan renames the table too, so the `RenameColumn`'s own
    /// `table` is the declared `app.customer` and the catalog still has
    /// `app.client` (DECISIONS 407).
    #[test]
    fn targets_are_taken_from_the_old_names() {
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().expect("a uid"),
                    from: tname("app.client"),
                    to: tname("app.customer"),
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().expect("a uid"),
                    table: tname("app.customer"),
                    from: "email".into(),
                    to: "contact_email".into(),
                }),
                PlannedChange::new(Change::DropTable {
                    uid: "t_bbbbbb".parse().expect("a uid"),
                    name: tname("app.old"),
                }),
            ],
        };
        assert_eq!(
            rename_targets(&cs),
            [
                RenameTarget::Table(tname("app.client")),
                RenameTarget::Column("app.client.email".parse().expect("a column")),
            ],
            "only renames, and by their old names — table and column alike"
        );
    }

    /// A column rename with no table rename beside it keeps the table it names.
    ///
    /// The negative half of the case above: the translation must be a lookup
    /// and not a rewrite, or a plan that renames only the column would be
    /// asked about under whatever table happened to be renamed elsewhere.
    #[test]
    fn a_column_rename_alone_keeps_the_table_it_names() {
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().expect("a uid"),
                    from: tname("app.order"),
                    to: tname("app.purchase"),
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().expect("a uid"),
                    table: tname("app.customer"),
                    from: "email".into(),
                    to: "contact_email".into(),
                }),
            ],
        };
        assert_eq!(
            rename_targets(&cs),
            [
                RenameTarget::Table(tname("app.order")),
                RenameTarget::Column("app.customer.email".parse().expect("a column")),
            ],
            "a table this plan does not rename is not translated"
        );
    }

    /// A module drop is not a target here: what points at a module about to go
    /// is `crate::modules`' question, and answering it twice would give two
    /// answers the first time either was fixed.
    #[test]
    fn a_module_drop_is_left_to_the_module_reader() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::DropModule {
                id: "app.active_customer".parse().expect("a module id"),
                kind: pbps_model::ModuleKind::View,
            })],
        };
        assert!(rename_targets(&cs).is_empty(), "{cs:#?}");
    }

    #[test]
    fn a_column_target_reports_its_own_table() {
        let target = RenameTarget::Column("app.customer.email".parse().expect("a column"));
        assert_eq!(target.table(), &tname("app.customer"));
        assert_eq!(target.to_string(), "column app.customer.email");
        assert_eq!(target.column(), Some("email"));
        assert_eq!(RenameTarget::Table(tname("app.customer")).column(), None);
    }

    /// The trap this function exists for: a prefix match would report every
    /// routine that mentions a similarly named column.
    #[test]
    fn a_body_names_the_column_only_where_the_name_stands_alone() {
        assert!(mentions("SELECT email FROM customer", "email"));
        assert!(mentions("SELECT \"email\" FROM customer", "email"));
        assert!(mentions("RETURN (SELECT email);", "email"));
        assert!(mentions("email", "email"));

        assert!(!mentions("SELECT email_address FROM t", "email"));
        assert!(!mentions("SELECT contact_email FROM t", "email"));
        assert!(!mentions("SELECT emailx FROM t", "email"));
        // A name whose first character is multibyte, embedded in a longer
        // identifier. The scan has to step over the rejected match by that
        // character's width: a one-byte step lands inside it and slicing the
        // body there panics (DECISIONS 408). Each of these is a body that once
        // took the process with it.
        assert!(!mentions("SELECT xä FROM t", "ä"));
        assert!(!mentions("SELECT äx FROM t", "ä"));
        assert!(mentions("SELECT ä FROM t", "ä"));
        assert!(!mentions("SELECT 王小明x FROM t", "王小明"));
        assert!(mentions("SELECT 王小明 FROM t", "王小明"));
        // And no name is no match, rather than a match at every position.
        assert!(!mentions("SELECT email FROM t", ""));

        // A dollar sign continues an identifier on this engine, where `@` and
        // `#` do not — which is the one place this parts company with the
        // SQL Server rule of the same shape.
        assert!(!mentions("SELECT email$1 FROM t", "email"));
        assert!(mentions("SELECT @email FROM t", "email"));
        // A non-ASCII byte continues one too: `emailä` is one name.
        assert!(!mentions("SELECT emailä FROM t", "email"));
    }

    /// The engine folds an *unquoted* identifier before storing it, so a body
    /// naming `EMAIL` or `Email` bare still names the column `email`
    /// (DECISIONS 230, 313). The trap is the negative case: a *quoted*
    /// `"EMAIL"` is a different column from `email`, case preserved, and
    /// folding it too would report a routine that does not actually break.
    #[test]
    fn an_unquoted_mention_folds_case_and_a_quoted_one_does_not() {
        assert!(mentions("SELECT EMAIL FROM customer", "email"));
        assert!(mentions("SELECT Email FROM customer", "email"));
        assert!(!mentions("SELECT \"EMAIL\" FROM customer", "email"));
        assert!(!mentions("SELECT \"Email\" FROM customer", "email"));

        // The quoted spelling still matches itself exactly, unfolded.
        assert!(mentions("SELECT \"EMAIL\" FROM customer", "EMAIL"));

        // A target whose own name carries an ASCII uppercase letter can only
        // be in the catalog because it was created quoted — no unquoted
        // spelling ever folds to it, so a bare, differently-cased mention
        // names a different, lower-spelled column and must not match.
        assert!(!mentions("SELECT EMAIL FROM customer", "Email"));
        assert!(!mentions("SELECT email FROM customer", "Email"));
        assert!(mentions("SELECT \"Email\" FROM customer", "Email"));

        // Bounded the same way a same-case match is: `EMAILX` is one longer
        // name, not this one, whatever the case.
        assert!(!mentions("SELECT EMAILX FROM t", "email"));

        // The regression this refactor could have introduced: a same-case
        // quoted mention is caught by the exact `"name"` check before the
        // case-folded scan ever runs, so its `reject_quoted` must never be
        // allowed to shadow that earlier, unconditional acceptance.
        assert!(mentions("SELECT \"email\" FROM customer", "email"));

        // The other side of the ASCII-only line (DECISIONS 230): an
        // unquoted `CREATE TABLE AÄ` still makes the relation `aÄ`, so a
        // target whose name carries a *non-ASCII* uppercase letter can come
        // from an unquoted spelling, and the fold must still run for it.
        // Guards this against being "simplified" to
        // `name.chars().all(char::is_lowercase)`, which would wrongly skip it.
        assert!(mentions("SELECT AÄ FROM t", "aÄ"));
    }

    /// The queries have to keep the two properties the module documentation
    /// rests on: text bodies are found by the engine's own record of which
    /// bodies it parsed, and the carried list does not report an object as its
    /// own dependant.
    #[test]
    fn the_queries_ask_the_engine_which_bodies_it_parsed() {
        assert!(
            TEXT_BODIED_ROUTINES.contains("p.prosqlbody IS NULL"),
            "a language list would put `sql` on both sides of the line"
        );
        assert!(
            TEXT_BODIED_ROUTINES.contains("d.deptype = 'e'"),
            "extensions"
        );
        // DECISIONS 449: an unqualified `lower()` runs under the database's
        // default collation, and on a Turkish one it folds `I` to dotless
        // `ı`, not `i` — measured, `strpos(lower('SELECT I FROM t'),
        // lower('i'))` is `0` where the ASCII fold gives `8`, excluding a
        // routine that really does mention the target before `mentions` ever
        // sees it. `COLLATE "C"` on both sides is what makes the fold ASCII
        // and locale-independent, matching the engine's own identifier fold
        // rather than approximating it. This cannot be exercised against the
        // live suite's own database without changing its collation, so it is
        // pinned here instead, the way the repo pins other spellings it
        // cannot exercise end to end.
        for side in ["p.prosrc COLLATE \"C\"", "$1 COLLATE \"C\""] {
            assert!(
                TEXT_BODIED_ROUTINES.contains(side),
                "the case-insensitive fold must run under the C collation on \
                 both sides, or a locale-specific default collation folds it \
                 differently from the engine's own identifier rule: {side}"
            );
        }
        assert!(
            CARRIED.contains("d.deptype <> 'i'"),
            "an internal edge is the object's own parts"
        );
        assert!(
            CARRIED.contains("d.refobjsubid IN (0, $2)"),
            "a whole-relation edge reaches a column rename too"
        );
        assert!(
            ATTNUM.contains("NOT a.attisdropped"),
            "a dropped column keeps its slot (ADR-0012 §6)"
        );
        // The *bound* name goes through `to_regclass` and never through a
        // cast: the cast raises for a name that is not there, and a raise is
        // not an empty report. A constant class name beside it is a different
        // thing and is left alone.
        for sql in [CARRIED, NAMED_OBJECTS, ATTNUM] {
            assert!(sql.contains("to_regclass($1)"), "{sql}");
            assert!(!sql.contains("$1::regclass"), "{sql}");
        }
    }

    #[test]
    fn an_empty_report_knows_it_is_empty() {
        let r = ImpactReport {
            target: "column app.customer.email".into(),
            ..Default::default()
        };
        assert!(r.is_empty());
        // And the note about what no query can see is printed even then: a
        // report with nothing in it is exactly when a reader most needs to be
        // told what it did not look at.
        assert_eq!(notes(&r).len(), 1, "{:?}", notes(&r));
    }
}
