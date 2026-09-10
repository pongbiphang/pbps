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

use pbps_db::{Conn, DbError, Row};
use pbps_dialect::DialectError;
use pbps_model::{ColumnRef, TableName};

use crate::emit::qualified;

/// What is being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Table(TableName),
    Column(ColumnRef),
}

impl RenameTarget {
    /// The table whose dependants have to be queried.
    pub fn table(&self) -> &TableName {
        match self {
            RenameTarget::Table(t) => t,
            RenameTarget::Column(c) => &c.table,
        }
    }

    /// The column, where the target is one.
    fn column(&self) -> Option<&str> {
        match self {
            RenameTarget::Table(_) => None,
            RenameTarget::Column(c) => Some(&c.name),
        }
    }

    /// Every rename in a plan, as the objects they are renamed **from** — the
    /// name the catalog still knows them by. Taking the new name would query
    /// something that does not exist yet and report no impact at all, which is
    /// the most dangerous possible answer.
    pub fn from_changes(changes: &pbps_model::ChangeSet) -> Vec<RenameTarget> {
        use pbps_model::Change;
        changes
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(RenameTarget::Table(from.clone())),
                Change::RenameColumn { table, from, .. } => {
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
}

impl std::fmt::Display for RenameTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameTarget::Table(t) => write!(f, "table {t}"),
            RenameTarget::Column(c) => write!(f, "column {c}"),
        }
    }
}

/// One object that refers to the thing being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    /// `function`, `procedure`, `view`, `index`, `constraint`, …
    pub kind: String,
    pub name: String,
    /// Why this one matters, when the kind alone does not say it.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactReport {
    pub target: String,

    /// Referrers the engine refuses the statement over. **Empty on this
    /// engine for a rename**, measured; the field is the abstraction's and the
    /// module documentation says why nothing fills it.
    pub blocking: Vec<Referrer>,

    /// Objects that will break, which the engine will not stop.
    pub advisory: Vec<Referrer>,

    /// Objects the rename is carried into, which keep working.
    pub carried: Vec<Referrer>,
}

impl ImpactReport {
    pub fn is_empty(&self) -> bool {
        self.blocking.is_empty() && self.advisory.is_empty() && self.carried.is_empty()
    }
}

/// Why a rename's impact could not be reported.
///
/// Two failures kept apart, because the whole point of this module is that an
/// empty report means "nothing depends on this". A name that cannot be written
/// and a query that could not run are both *unknown*, and neither may arrive at
/// a caller wearing the shape of "no dependants".
#[derive(Debug, thiserror::Error)]
pub enum ImpactError {
    #[error(transparent)]
    Query(#[from] DbError),

    /// The target cannot be named: its own name cannot be written as an
    /// identifier, or this database has no such column to report on. Both are
    /// questions that could not be asked, and neither is an answer.
    #[error(transparent)]
    Name(#[from] DialectError),
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
   AND pg_catalog.strpos(p.prosrc, $1) > 0
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
fn mentions(body: &str, name: &str) -> bool {
    if body.contains(&format!("\"{name}\"")) {
        return true;
    }
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(offset) = body[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let before = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before && after {
            return true;
        }
        from = start + 1;
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
            RenameTarget::from_changes(&cs),
            [
                RenameTarget::Table(tname("app.client")),
                RenameTarget::Column("app.customer.email".parse().expect("a column")),
            ],
            "only renames, and by their old names"
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
        assert!(RenameTarget::from_changes(&cs).is_empty(), "{cs:#?}");
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
        // A dollar sign continues an identifier on this engine, where `@` and
        // `#` do not — which is the one place this parts company with the
        // SQL Server rule of the same shape.
        assert!(!mentions("SELECT email$1 FROM t", "email"));
        assert!(mentions("SELECT @email FROM t", "email"));
        // A non-ASCII byte continues one too: `emailä` is one name.
        assert!(!mentions("SELECT emailä FROM t", "email"));
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
