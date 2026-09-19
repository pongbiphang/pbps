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
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => None,
        })
        .collect()
}

/// Every routine in this database whose body the engine keeps as **text**,
/// with the body so [`mentions`] can match identifier tokens.
///
/// `prosqlbody IS NULL` is the whole test, and it is the engine's own record of
/// which bodies it parsed — measured, a `BEGIN ATOMIC` SQL function has one and
/// a `plpgsql` function and a plain SQL function do not. A language list would
/// have been the obvious spelling and would be wrong twice: `sql` appears on
/// both sides of the line, and a procedural language installed later is not on
/// any list written today.
///
/// A raw substring filter is not a superset of identifier matching: the
/// catalog name `a"b` is spelled `"a""b"` inside `prosrc`, so even a correctly
/// folded substring search loses it before the lexer sees it (DECISIONS 477).
/// Keep the lexical rule in Rust, using the dialect's existing literal and
/// quoting rules, rather than implementing a second lexer in this query.
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
   AND d.refobjid = ($1::int8)::oid
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
 WHERE i.indrelid = ($1::int8)::oid
UNION ALL
SELECT 'constraint', con.conname
  FROM pg_catalog.pg_constraint con
 WHERE con.conrelid = ($1::int8)::oid
 ORDER BY 1, 2";

/// The relation the report is about, resolved **once**, before any of it is
/// built.
///
/// `to_regclass` rather than a `::regclass` cast, and the reason is not the one
/// this module used to give. The cast raises, and its raise would reach a
/// caller as `ImpactError::Query` — "a query could not be run" — for a name
/// that was simply not there, which is a different answer from the one that is
/// true. The function answers NULL, and the NULL is read here and turned into
/// the refusal below, in this module's own words.
///
/// The oid travels on as `int8` and not `int4`: an oid is unsigned 32-bit, and
/// measured on 18.6 `4000000000::oid::int4` is `-294967296` while
/// `::int8` is `4000000000`. Every query after this one takes that oid, so an
/// absent name cannot reach them at all — a relation dropped and recreated
/// between two of them cannot make half the report about one object and half
/// about another either (DECISIONS 519, #269).
///
/// The queries that take it spell the cast `($1::int8)::oid` rather than
/// comparing an oid column to a bare `$1`: a bare one makes the engine infer
/// the parameter as `oid`, which the driver cannot serialize an `i64` into.
/// Measured, the cast keeps their index scans.
const RELATION: &str = "SELECT pg_catalog.to_regclass($1)::oid::int8 AS oid";

/// The column's `attnum`, which is what `pg_depend` records — **not** its
/// position: a dropped column keeps its slot, so the two part company the first
/// time anybody drops one (ADR-0012 §6).
///
/// No existence question of its own: [`RELATION`] has already established that
/// the relation is there, so an empty result here means the column is not, and
/// nothing else.
const ATTNUM: &str = "\
SELECT a.attnum::int4 AS attnum
  FROM pg_catalog.pg_attribute a
 WHERE a.attrelid = ($1::int8)::oid
   AND a.attname = $2 AND NOT a.attisdropped";

/// Queries every impact of one rename.
pub async fn rename_impact(
    conn: &mut Conn,
    target: &RenameTarget,
) -> Result<ImpactReport, ImpactError> {
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
    // The quoted, qualified spelling, because `RELATION` hands the name to
    // `to_regclass`, which *parses* its argument.
    let name = qualified(target.table())?;
    // Resolved before anything is built, and for a table target as much as for
    // a column one. A table target used to skip this: `to_regclass` answered
    // NULL inside every query, all of them joined to nothing, and the operator
    // was told a rename affects nothing about a rename that could not be
    // evaluated at all. A `RenameTable`'s `from` is by construction a name the
    // catalog had when the plan was made, so its absence means something
    // happened, and absent is not empty (DECISIONS 519, #269).
    let relation: i64 = match conn
        .query_with(RELATION, &[name.as_str().into()])
        .await?
        .first()
        .and_then(|row| row.try_get::<i64>("oid").transpose())
        .transpose()?
    {
        Some(oid) => oid,
        None => {
            return Err(ImpactError::Name(DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message: format!(
                    "{} is not a table this database has, so what a rename of {target} \
                     would affect cannot be read",
                    target.table()
                ),
            }));
        }
    };
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
                .query_with(ATTNUM, &[relation.into(), name.into()])
                .await?;
            match rows.first() {
                Some(row) => row.try_get::<i32>("attnum")?.unwrap_or(0),
                // The table is there and the column is not. That is not
                // "nothing depends on it": it is a question that could not be
                // asked, and the caller has to hear it as one.
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
    for row in conn.query(TEXT_BODIED_ROUTINES).await? {
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
        for row in conn.query_with(NAMED_OBJECTS, &[relation.into()]).await? {
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
        .query_with(CARRIED, &[relation.into(), attnum.into()])
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

/// Whether a stored body contains an identifier naming this object.
///
/// Literals and comments carry data, and a quoted identifier is one whole
/// token even when its contents look like several bare words (DECISIONS 477).
/// Use the dialect's literal scanner and emitter's quoting rule so embedded
/// quotes and PostgreSQL string forms cannot disagree with emitted SQL.
///
/// Unquoted names fold ASCII only (DECISIONS 230, 313, 448). A catalog name
/// with an ASCII uppercase letter can only have been created quoted; bare
/// spellings therefore cannot name it. Non-ASCII uppercase letters remain
/// unchanged by the engine and must not disable the ASCII fold.
///
/// A reserved word is a name only when quoted or following a dot. Whitespace
/// and masked comments can separate that dot from its identifier. Advancing
/// by complete characters keeps non-ASCII names intact (DECISIONS 408).
fn mentions(body: &str, name: &str) -> bool {
    let Ok(quoted_name) = crate::quote(name) else {
        return false;
    };
    let code = crate::LEXICON.code_only(body);
    let bare_name = name == name.to_ascii_lowercase();
    let reserved = crate::types::is_reserved(name);
    let mut rest = code.as_str();
    let mut after_dot = false;
    while let Some(ch) = rest.chars().next() {
        if ch == '"' {
            let Some(len) = crate::types::quoted_len(rest) else {
                break;
            };
            if rest[..len] == quoted_name {
                return true;
            }
            rest = &rest[len..];
            after_dot = false;
        } else if pbps_dialect::continues_ident(ch) {
            let len = rest
                .find(|c| !pbps_dialect::continues_ident(c))
                .unwrap_or(rest.len());
            if bare_name && (!reserved || after_dot) && rest[..len].eq_ignore_ascii_case(name) {
                return true;
            }
            rest = &rest[len..];
            after_dot = false;
        } else {
            if !(ch.is_ascii() && ch.is_whitespace()) {
                after_dot = ch == '.';
            }
            rest = &rest[ch.len_utf8()..];
        }
    }
    false
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

    #[test]
    fn embedded_quotes_in_impact_names_use_the_emitters_spelling() {
        assert!(mentions(r#"SELECT "a""b" FROM t"#, "a\"b"));
        assert!(!mentions(r#"SELECT "ab" FROM t"#, "a\"b"));
        assert!(!mentions(r#"SELECT "a""""b" FROM t"#, "a\"b"));
    }

    #[test]
    fn bare_reserved_words_are_not_impact_references() {
        for body in ["select 1", "SELECT 1", "SELECT other FROM t"] {
            assert!(!mentions(body, "select"), "{body}");
        }
        for body in [
            r#"SELECT "select" FROM t"#,
            "SELECT t.select FROM t",
            "SELECT t . SELECT FROM t",
            "SELECT t /* gap */ . select FROM t",
            "SELECT t .\u{000b}select FROM t",
        ] {
            assert!(mentions(body, "select"), "{body}");
        }
    }

    #[test]
    fn quoted_impact_references_match_only_the_complete_identifier() {
        for body in [
            r#"SELECT "other email" FROM t"#,
            r#"SELECT "other EMAIL" FROM t"#,
            r#"SELECT "email other" FROM t"#,
            r#"SELECT "other ""email""" FROM t"#,
        ] {
            assert!(!mentions(body, "email"), "{body}");
        }
        assert!(mentions(r#"SELECT "email" FROM t"#, "email"));
        assert!(mentions(r#"SELECT "other email", email FROM t"#, "email"));
        assert!(!mentions(r#"SELECT "unfinished email"#, "email"));
    }

    #[test]
    fn impact_references_ignore_literals_and_comments() {
        for body in [
            "RAISE NOTICE 'email';",
            "RAISE NOTICE 'EMAIL';",
            r"SELECT E'quote\' EMAIL';",
            "SELECT $$email$$;",
            "SELECT $message$EMAIL$message$;",
            r"SELECT U&'email';",
            "-- email\nSELECT 1",
            "/* email /* EMAIL */ email */ SELECT 1",
        ] {
            assert!(!mentions(body, "email"), "{body}");
        }
        assert!(mentions(
            "SELECT email FROM t; RAISE NOTICE 'unrelated';",
            "email"
        ));
        assert!(mentions("-- email\nSELECT EMAIL FROM t", "email"));
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

        // A same-case quoted spelling still names the target, independently
        // of the separate rule for folding bare identifiers.
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
        // Exactly one query resolves the bound name, and through
        // `to_regclass` rather than a cast: the cast raises for a name that is
        // not there, and a raise reaches a caller as `ImpactError::Query` —
        // "a query could not be run" — where the true answer is that the name
        // is absent. A constant class name beside it is a different thing and
        // is left alone.
        assert!(RELATION.contains("to_regclass($1)"), "{RELATION}");
        assert!(!RELATION.contains("$1::regclass"), "{RELATION}");
        // And every other query takes the oid that one produced, so no name a
        // report is built from can resolve to nothing halfway through (#269).
        for sql in [CARRIED, NAMED_OBJECTS, ATTNUM] {
            assert!(!sql.contains("to_regclass"), "{sql}");
            assert!(sql.contains("($1::int8)::oid"), "{sql}");
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
