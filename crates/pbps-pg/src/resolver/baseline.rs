//! Running a resolver's baseline on scratch, and naming what it created
//! (SPEC §9.3.2; #1664, #1673).
//!
//! The baseline is a reviewed SQL file that creates the objects outside the
//! managed set that managed objects reference. It runs on scratch only, after
//! the authorization reproduction and before any managed object, one
//! statement at a time, so a statement that fails is named with the engine's
//! error. What it created is the difference between two inventories taken
//! around it; each root of that difference is then compared with the target.

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};
use std::collections::{BTreeMap, BTreeSet};

/// One statement of a baseline, as written, with the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub line: usize,
}

/// A baseline the splitter cannot read: an unterminated quote or comment
/// would otherwise swallow every statement after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub line: usize,
    pub what: &'static str,
}

/// Splits a baseline into its statements the way `psql` does: a `;` ends a
/// statement outside quotes, comments and parentheses, and outside the
/// `BEGIN ATOMIC ... END` body of a `CREATE FUNCTION` or `CREATE PROCEDURE`.
/// Text that is only whitespace and comments is no statement.
pub fn split(sql: &str) -> Result<Vec<Statement>, Unreadable> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut line = 1;
    let mut start_line = 1;
    // Whether anything but whitespace and comments has been seen since
    // `start`.
    let mut substantive = false;
    let mut parens = 0usize;
    let mut words: Vec<String> = Vec::new();
    let mut begin_depth = 0usize;
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied();
    let identifier_start = |c: char| c.is_alphabetic() || c == '_' || !c.is_ascii();
    let identifier_part = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
        }
        match c {
            '-' if at(i + 1) == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if at(i + 1) == Some('*') => {
                let opened = line;
                let mut depth = 0usize;
                loop {
                    match (at(i), at(i + 1)) {
                        (Some('/'), Some('*')) => {
                            depth += 1;
                            i += 2;
                        }
                        (Some('*'), Some('/')) => {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        (Some(c), _) => {
                            if c == '\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                        (None, _) => {
                            return Err(Unreadable {
                                line: opened,
                                what: "a comment that is never closed",
                            });
                        }
                    }
                }
                continue;
            }
            '\'' => {
                // `E'...'` takes backslash escapes; any other string only
                // doubles its quote.
                let escapes = i > 0
                    && matches!(chars[i - 1], 'e' | 'E')
                    && (i < 2 || !identifier_part(chars[i - 2]));
                let opened = line;
                i += 1;
                loop {
                    match at(i) {
                        None => {
                            return Err(Unreadable {
                                line: opened,
                                what: "a string that is never closed",
                            });
                        }
                        Some('\\') if escapes => i += 2,
                        Some('\'') if at(i + 1) == Some('\'') => i += 2,
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                    }
                }
                substantive = true;
                continue;
            }
            '"' => {
                let opened = line;
                i += 1;
                loop {
                    match at(i) {
                        None => {
                            return Err(Unreadable {
                                line: opened,
                                what: "a quoted name that is never closed",
                            });
                        }
                        Some('"') if at(i + 1) == Some('"') => i += 2,
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                    }
                }
                substantive = true;
                continue;
            }
            '$' if i == 0 || !identifier_part(chars[i - 1]) => {
                // `$tag$ ... $tag$`; `$1` is a parameter, not a quote.
                let mut end = i + 1;
                while end < chars.len() && identifier_part(chars[end]) && chars[end] != '$' {
                    end += 1;
                }
                let tagged =
                    at(end) == Some('$') && (end == i + 1 || identifier_start(chars[i + 1]));
                if tagged {
                    let tag: String = chars[i..=end].iter().collect();
                    let opened = line;
                    let body = end + 1;
                    let rest: String = chars[body..].iter().collect();
                    let Some(found) = rest.find(&tag) else {
                        return Err(Unreadable {
                            line: opened,
                            what: "a dollar-quoted string that is never closed",
                        });
                    };
                    let closed = body + rest[..found].chars().count();
                    line += chars[body..closed].iter().filter(|&&c| c == '\n').count();
                    i = closed + tag.chars().count();
                    substantive = true;
                    continue;
                }
            }
            '(' => parens += 1,
            ')' => parens = parens.saturating_sub(1),
            ';' if parens == 0 && begin_depth == 0 => {
                if substantive {
                    statements.push(Statement {
                        text: chars[start..i].iter().collect::<String>().trim().to_owned(),
                        line: start_line,
                    });
                }
                start = i + 1;
                substantive = false;
                words.clear();
                i += 1;
                continue;
            }
            c if identifier_start(c) => {
                let mut end = i + 1;
                while end < chars.len() && identifier_part(chars[end]) {
                    end += 1;
                }
                let word = chars[i..end]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                if !substantive {
                    start_line = line;
                    substantive = true;
                }
                // psql's rule: inside a routine's definition, `BEGIN` and
                // `CASE` open a block that `END` closes, so a SQL-standard
                // body's own `;` do not end the statement.
                if creates_routine(&words) {
                    match word.as_str() {
                        "begin" | "case" => begin_depth += 1,
                        "end" => begin_depth = begin_depth.saturating_sub(1),
                        _ => {}
                    }
                }
                if words.len() < 4 {
                    words.push(word);
                }
                i = end;
                continue;
            }
            _ => {}
        }
        if !c.is_whitespace() && !substantive {
            start_line = line;
            substantive = true;
        }
        i += 1;
    }
    if substantive {
        statements.push(Statement {
            text: chars[start..].iter().collect::<String>().trim().to_owned(),
            line: start_line,
        });
    }
    Ok(statements)
}

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE`, from a statement's first words.
fn creates_routine(words: &[String]) -> bool {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        words.as_slice(),
        ["create", "function" | "procedure", ..]
            | ["create", "or", "replace", "function" | "procedure", ..]
    )
}

/// A baseline statement the engine refused, with its error.
#[derive(Debug)]
pub struct Failed {
    pub statement: Statement,
    pub error: DbError,
}

impl Failed {
    /// Whether the engine named something that does not exist. A baseline
    /// runs before any managed object, so such a name may be a managed
    /// object it may not refer to.
    pub fn names_something_missing(&self) -> bool {
        matches!(
            self.error.server_error_code().as_deref(),
            Some("42P01" | "42883" | "42704" | "3F000" | "42703")
        )
    }
}

/// Runs each statement on its own, in order, and stops at the first the
/// engine refuses. Statements are not wrapped in a transaction: a baseline
/// may hold its own, and the database is the run's to discard.
pub async fn run(
    conn: &mut impl ExecuteConnection,
    statements: &[Statement],
) -> Result<(), Failed> {
    for statement in statements {
        if let Err(error) = conn.execute(&statement.text).await {
            return Err(Failed {
                statement: statement.clone(),
                error,
            });
        }
    }
    Ok(())
}

/// One object in the scratch database: its catalog, its OID there, and the
/// engine's name for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Object {
    pub catalog: String,
    pub oid: u32,
    pub described: String,
}

/// What a baseline created, taken apart into roots and what they carry.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Created {
    /// Created objects that live on no other created object: what is
    /// compared with the target.
    pub roots: BTreeSet<Object>,
    /// Members of an extension the baseline created, which its version
    /// covers.
    pub members: usize,
}

/// Every object in the connected database that initdb did not create, each
/// with whether it lives on another object: internally (an index of a
/// constraint, a table's row type, an array type), automatically (an index,
/// an owned sequence) or as an extension member. Measured on 18.
pub async fn inventory(conn: &mut impl QueryConnection) -> Result<Inventory, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT f.classid::pg_catalog.regclass::text AS catalog, f.objid::text AS oid, \
                    pg_catalog.pg_describe_object(f.classid, f.objid, 0) AS described, \
                    (SELECT pg_catalog.min(d.deptype::text) FROM pg_catalog.pg_depend d \
                      WHERE d.classid = f.classid AND d.objid = f.objid \
                        AND d.deptype IN ('i', 'a', 'e')) AS carried \
               FROM ({}) f",
            super::vouched::user_objects()
        ))
        .await?;
    rows.iter()
        .map(|row| {
            let oid = text(row, "oid")?
                .parse()
                .map_err(|_| DbError::BadRow("an object OID was not a number".into()))?;
            let carried = row
                .try_get::<&str>("carried")?
                .and_then(|kind| kind.chars().next());
            Ok((
                (text(row, "catalog")?, oid),
                (text(row, "described")?, carried),
            ))
        })
        .collect()
}

/// Every object initdb did not create, by catalog and OID, with its
/// description and how it lives on another object, if it does.
pub type Inventory = BTreeMap<(String, u32), (String, Option<char>)>;

/// What appeared between two inventories: its roots, and how many extension
/// members came with them.
pub fn created(before: &Inventory, after: &Inventory) -> Created {
    let mut created = Created::default();
    for ((catalog, oid), (described, carried)) in after {
        if before.contains_key(&(catalog.clone(), *oid)) {
            continue;
        }
        match carried {
            None => {
                created.roots.insert(Object {
                    catalog: catalog.clone(),
                    oid: *oid,
                    described: described.clone(),
                });
            }
            Some('e') => created.members += 1,
            Some(_) => {}
        }
    }
    created
}

/// Whether the session's role may use each named schema, as the engine
/// answers it. A schema the database lacks is left out.
pub async fn usage(
    conn: &mut impl QueryConnection,
    schemas: &[String],
) -> Result<BTreeMap<String, bool>, DbError> {
    if schemas.is_empty() {
        return Ok(BTreeMap::new());
    }
    let names = schemas
        .iter()
        .map(|schema| super::authorization::setting_literal(schema))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = conn
        .query(&format!(
            "SELECT n.nspname::text AS schema, \
                    pg_catalog.has_schema_privilege(n.oid, 'USAGE')::text AS usable \
               FROM pg_catalog.pg_namespace n WHERE n.nspname IN ({names})"
        ))
        .await?;
    rows.iter()
        .map(|row| Ok((text(row, "schema")?, text(row, "usable")? == "true")))
        .collect()
}

/// Gives `deployer` USAGE on each schema where `wanted` says the target's
/// deployer has it, and takes PUBLIC's away where it does not, as the run's
/// administrator (#1673). A schema the baseline created is reproduced by the
/// same answer as an in-scope one, not left to the baseline to grant: the
/// baseline cannot know a run-owned deployer's generated name.
pub async fn reproduce_usage(
    conn: &mut impl ExecuteConnection,
    deployer: &str,
    wanted: &BTreeMap<String, bool>,
) -> Result<(), DbError> {
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    for (schema, usable) in wanted {
        conn.execute(&if *usable {
            format!(
                "GRANT USAGE ON SCHEMA {} TO {}",
                quote(schema),
                quote(deployer)
            )
        } else {
            format!("REVOKE USAGE ON SCHEMA {} FROM PUBLIC", quote(schema))
        })
        .await?;
    }
    Ok(())
}

/// One link of a possible chain through the boundary, as the target
/// records it: `binder` depends on `middle`, whose compared shape names the
/// relation `named` (a column or argument of its row type, a parent, a cast
/// over it, a domain over it). Each is the engine's description and its
/// address names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub binder: Address,
    pub middle: Address,
    pub named: Address,
}

/// An object as `pg_identify_object_as_address` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub described: String,
    pub kind: String,
    pub names: Vec<String>,
}

/// Every one-hop link through a relation's row type or a parent on the
/// connected database, from its recorded dependencies. A view's query, a
/// routine's body and a relation's defaults, constraints, triggers and
/// policies bind, so they make a binder, but they are no shape, so never a
/// middle (SPEC §9.3.2). Measured on 18.
pub async fn links(conn: &mut impl QueryConnection) -> Result<Vec<Link>, DbError> {
    let rows = conn.query(LINKS).await?;
    let address = |row: &Row, prefix: &str| -> Result<Address, DbError> {
        Ok(Address {
            described: text(row, &format!("{prefix}_described"))?,
            kind: text(row, &format!("{prefix}_kind"))?,
            // As JSON: the driver reads no arrays, and a name may hold a
            // comma or a quote.
            names: serde_json::from_str(&text(row, &format!("{prefix}_names"))?)
                .map_err(|_| DbError::BadRow(format!("{prefix}_names was no list of names")))?,
        })
    };
    rows.iter()
        .map(|row| {
            Ok(Link {
                binder: address(row, "binder")?,
                middle: address(row, "middle")?,
                named: address(row, "named")?,
            })
        })
        .collect()
}

const LINKS: &str = "\
WITH RECURSIVE \
rowtype AS ( \
  SELECT t.oid AS type, t.typrelid AS relation FROM pg_catalog.pg_type t WHERE t.typrelid <> 0 \
  UNION ALL \
  SELECT t.oid, r.relation FROM pg_catalog.pg_type t JOIN rowtype r ON r.type = t.typelem \
   WHERE t.typsubscript = 'pg_catalog.array_subscript_handler'::pg_catalog.regproc \
), \
shape AS ( \
  SELECT 'pg_catalog.pg_class'::pg_catalog.regclass AS classid, a.attrelid AS objid, r.relation \
    FROM pg_catalog.pg_attribute a JOIN rowtype r ON r.type = a.atttypid \
   WHERE a.attnum > 0 AND NOT a.attisdropped \
  UNION ALL \
  SELECT 'pg_catalog.pg_class'::pg_catalog.regclass, i.inhrelid, i.inhparent \
    FROM pg_catalog.pg_inherits i \
  UNION ALL \
  SELECT 'pg_catalog.pg_proc'::pg_catalog.regclass, p.oid, r.relation \
    FROM pg_catalog.pg_proc p \
    JOIN rowtype r ON r.type = p.prorettype \
                   OR r.type = ANY (p.proargtypes::pg_catalog.oid[]) \
                   OR r.type = ANY (p.proallargtypes) \
  UNION ALL \
  SELECT 'pg_catalog.pg_cast'::pg_catalog.regclass, c.oid, r.relation \
    FROM pg_catalog.pg_cast c JOIN rowtype r ON r.type IN (c.castsource, c.casttarget) \
  UNION ALL \
  SELECT 'pg_catalog.pg_type'::pg_catalog.regclass, t.oid, r.relation \
    FROM pg_catalog.pg_type t JOIN rowtype r ON r.type = t.typbasetype WHERE t.typtype = 'd' \
), \
binder AS ( \
  SELECT d.refclassid, d.refobjid, \
         CASE WHEN d.classid IN ('pg_catalog.pg_rewrite'::pg_catalog.regclass, \
                                 'pg_catalog.pg_attrdef'::pg_catalog.regclass, \
                                 'pg_catalog.pg_trigger'::pg_catalog.regclass, \
                                 'pg_catalog.pg_policy'::pg_catalog.regclass, \
                                 'pg_catalog.pg_constraint'::pg_catalog.regclass) \
              THEN 'pg_catalog.pg_class'::pg_catalog.regclass ELSE d.classid END AS classid, \
         CASE d.classid \
           WHEN 'pg_catalog.pg_rewrite'::pg_catalog.regclass \
             THEN (SELECT w.ev_class FROM pg_catalog.pg_rewrite w WHERE w.oid = d.objid) \
           WHEN 'pg_catalog.pg_attrdef'::pg_catalog.regclass \
             THEN (SELECT f.adrelid FROM pg_catalog.pg_attrdef f WHERE f.oid = d.objid) \
           WHEN 'pg_catalog.pg_trigger'::pg_catalog.regclass \
             THEN (SELECT g.tgrelid FROM pg_catalog.pg_trigger g WHERE g.oid = d.objid) \
           WHEN 'pg_catalog.pg_policy'::pg_catalog.regclass \
             THEN (SELECT o.polrelid FROM pg_catalog.pg_policy o WHERE o.oid = d.objid) \
           WHEN 'pg_catalog.pg_constraint'::pg_catalog.regclass \
             THEN (SELECT n.conrelid FROM pg_catalog.pg_constraint n WHERE n.oid = d.objid) \
           ELSE d.objid END AS objid \
    FROM pg_catalog.pg_depend d WHERE d.deptype = 'n' \
), \
link AS ( \
  SELECT DISTINCT b.classid AS binder_class, b.objid AS binder_oid, \
         s.classid AS middle_class, s.objid AS middle_oid, s.relation \
    FROM shape s \
    JOIN binder b ON (b.refclassid = s.classid AND b.refobjid = s.objid) \
                  OR (s.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
                      AND b.refclassid = 'pg_catalog.pg_type'::pg_catalog.regclass \
                      AND b.refobjid = (SELECT c.reltype FROM pg_catalog.pg_class c \
                                         WHERE c.oid = s.objid)) \
   WHERE s.objid >= 16384 AND b.objid IS NOT NULL AND b.objid <> 0 \
     AND NOT (b.classid = s.classid AND b.objid = s.objid) \
) \
SELECT pg_catalog.pg_describe_object(l.binder_class, l.binder_oid, 0) AS binder_described, \
       bi.type AS binder_kind, pg_catalog.array_to_json(bi.object_names)::text AS binder_names, \
       pg_catalog.pg_describe_object(l.middle_class, l.middle_oid, 0) AS middle_described, \
       mi.type AS middle_kind, pg_catalog.array_to_json(mi.object_names)::text AS middle_names, \
       pg_catalog.pg_describe_object('pg_catalog.pg_class'::pg_catalog.regclass, l.relation, 0) \
         AS named_described, \
       ni.type AS named_kind, pg_catalog.array_to_json(ni.object_names)::text AS named_names \
  FROM link l \
  CROSS JOIN LATERAL pg_catalog.pg_identify_object_as_address(l.binder_class, l.binder_oid, 0) bi \
  CROSS JOIN LATERAL pg_catalog.pg_identify_object_as_address(l.middle_class, l.middle_oid, 0) mi \
  CROSS JOIN LATERAL pg_catalog.pg_identify_object_as_address( \
    'pg_catalog.pg_class'::pg_catalog.regclass, l.relation, 0) ni \
 ORDER BY 1, 4, 7";

/// The chains among `links`: a binder `desired` keeps, through an object
/// outside the managed set, to a relation either side manages. Each names
/// its remedies (SPEC §9.3.2).
pub fn chains(
    links: &[Link],
    desired: &super::capture::Managed,
    base: &super::capture::Managed,
) -> Vec<String> {
    let relation = |managed: &super::capture::Managed, address: &Address| match (
        address.kind.as_str(),
        address.names.as_slice(),
    ) {
        (
            "table" | "view" | "materialized view" | "foreign table" | "composite type"
            | "sequence" | "index",
            [schema, name],
        ) => managed.relation(schema, name),
        ("function" | "procedure" | "aggregate", [schema, name]) => managed.routine(schema, name),
        _ => false,
    };
    let managed = |address: &Address| relation(desired, address) || relation(base, address);
    links
        .iter()
        .filter(|link| {
            relation(desired, &link.binder) && !managed(&link.middle) && managed(&link.named)
        })
        .map(|link| {
            format!(
                "{} binds {}, whose shape names the managed {}: no baseline can stage it before \
                 the managed objects exist; adopt {} into the managed set too, or select no \
                 resolver and keep ADR-0013's conservative rebuild",
                link.binder.described,
                link.middle.described,
                link.named.described,
                link.middle.described
            )
        })
        .collect()
}

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("{field} was NULL")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<String> {
        split(sql).unwrap().into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn a_baseline_splits_at_each_top_level_semicolon() {
        assert_eq!(
            texts("CREATE SCHEMA ext;\nCREATE TABLE ext.t (a int, b text);\n"),
            ["CREATE SCHEMA ext", "CREATE TABLE ext.t (a int, b text)"]
        );
        // The last statement needs no `;`.
        assert_eq!(texts("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
        // Negative: comments and blank statements are no statements.
        assert!(texts("-- nothing\n/* still nothing */ ;;\n").is_empty());
    }

    #[test]
    fn a_semicolon_inside_quotes_comments_or_parentheses_ends_nothing() {
        assert_eq!(
            texts(
                "SELECT 'a;b', \"c;d\", E'e\\';f' -- g;h\n; /* i; /* nested; */ j; */ \
                 SELECT $$k;l$$, $tag$m;$$;n$tag$, $1;"
            ),
            [
                "SELECT 'a;b', \"c;d\", E'e\\';f' -- g;h",
                "/* i; /* nested; */ j; */ SELECT $$k;l$$, $tag$m;$$;n$tag$, $1"
            ]
        );
        // `''` doubles a quote; a plain string has no backslash escapes.
        assert_eq!(
            texts("SELECT 'it''s;'; SELECT 'a\\'; SELECT 2"),
            ["SELECT 'it''s;'", "SELECT 'a\\'", "SELECT 2"]
        );
    }

    #[test]
    fn a_sql_standard_routine_body_is_one_statement() {
        assert_eq!(
            texts(
                "CREATE OR REPLACE FUNCTION ext.f(x int) RETURNS int LANGUAGE sql\n\
                 BEGIN ATOMIC\n  SELECT CASE WHEN x > 0 THEN 1 ELSE 0 END;\n  SELECT 2;\nEND;\n\
                 CREATE PROCEDURE ext.p() LANGUAGE sql BEGIN ATOMIC SELECT 1; END;\n\
                 BEGIN; SELECT 3; END;"
            ),
            [
                "CREATE OR REPLACE FUNCTION ext.f(x int) RETURNS int LANGUAGE sql\n\
                 BEGIN ATOMIC\n  SELECT CASE WHEN x > 0 THEN 1 ELSE 0 END;\n  SELECT 2;\nEND",
                "CREATE PROCEDURE ext.p() LANGUAGE sql BEGIN ATOMIC SELECT 1; END",
                // Negative: a transaction's `BEGIN` opens no block.
                "BEGIN",
                "SELECT 3",
                "END"
            ]
        );
    }

    #[test]
    fn each_statement_carries_the_line_it_starts_on() {
        let statements =
            split("-- header\n\nCREATE SCHEMA ext;\n\n  /* c */\nSELECT\n 1;").unwrap();
        assert_eq!(
            statements.iter().map(|s| s.line).collect::<Vec<_>>(),
            [3, 6]
        );
    }

    #[test]
    fn an_unterminated_quote_or_comment_is_named_where_it_opens() {
        for (sql, line, what) in [
            (
                "SELECT 1;\nSELECT 'open",
                2,
                "a string that is never closed",
            ),
            ("SELECT \"open", 1, "a quoted name that is never closed"),
            ("SELECT 1;\n\n/* open", 3, "a comment that is never closed"),
            (
                "SELECT $x$ open",
                1,
                "a dollar-quoted string that is never closed",
            ),
        ] {
            assert_eq!(split(sql), Err(Unreadable { line, what }), "{sql}");
        }
    }

    #[test]
    fn what_a_baseline_created_is_its_roots_without_what_they_carry() {
        let object = |catalog: &str, oid, described: &str, carried| {
            ((catalog.to_owned(), oid), (described.to_owned(), carried))
        };
        let before = BTreeMap::from([object("pg_namespace", 16384, "schema app", None)]);
        let after = BTreeMap::from([
            object("pg_namespace", 16384, "schema app", None),
            object("pg_namespace", 16390, "schema ext", None),
            object("pg_class", 16391, "table ext.t", None),
            object("pg_type", 16393, "type ext.t", Some('i')),
            object("pg_class", 16394, "index ext.t_pkey", Some('i')),
            object("pg_extension", 16400, "extension citext", None),
            object("pg_type", 16401, "type citext", Some('e')),
        ]);
        let created = created(&before, &after);
        assert_eq!(
            created
                .roots
                .iter()
                .map(|o| o.described.as_str())
                .collect::<Vec<_>>(),
            // Ordered by catalog, then OID.
            ["table ext.t", "extension citext", "schema ext"]
        );
        assert_eq!(created.members, 1);
        // Negative: nothing new, nothing created.
        assert_eq!(created_none(&after), Created::default());
    }

    fn created_none(inventory: &BTreeMap<(String, u32), (String, Option<char>)>) -> Created {
        created(inventory, inventory)
    }

    #[tokio::test]
    #[ignore = "requires the pinned PostgreSQL servers"]
    async fn a_baseline_runs_statement_by_statement_and_its_roots_are_found() {
        use pbps_db::{Conn, Driver};
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
            let name = format!(
                "pbps_base1673_{}",
                crate::catalog::probe_token().replace('-', "_")
            );
            let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
            admin
                .execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
                .await
                .unwrap();
            let mut conn = Conn::connect(Driver::Postgres, &format!("{base} dbname={name}"))
                .await
                .unwrap();
            conn.execute("CREATE SCHEMA app").await.unwrap();
            let before = inventory(&mut conn).await.unwrap();
            let statements = split(
                "CREATE SCHEMA ext;\n\
                 CREATE TABLE ext.t (id serial PRIMARY KEY, a int UNIQUE, b text);\n\
                 CREATE INDEX ON ext.t (b);\n\
                 CREATE VIEW ext.v AS SELECT NULL::integer AS id WHERE false;\n\
                 CREATE FUNCTION ext.f(x int) RETURNS int LANGUAGE sql\n\
                 BEGIN ATOMIC SELECT x; END;\n\
                 CREATE TYPE ext.r AS RANGE (subtype = int4);",
            )
            .unwrap();
            let ran = run(&mut conn, &statements).await;
            let after = inventory(&mut conn).await.unwrap();
            // Negative: a statement naming what does not exist is refused
            // by name, and stops the run there.
            let missing =
                split("CREATE VIEW ext.w AS SELECT * FROM app.t;\nCREATE SCHEMA never;").unwrap();
            let failed = run(&mut conn, &missing).await.unwrap_err();
            drop(conn);
            admin
                .execute(&format!("DROP DATABASE {name}"))
                .await
                .unwrap();
            assert!(ran.is_ok(), "{variable}: {:?}", ran.err().map(|f| f.error));
            let mut roots = created(&before, &after)
                .roots
                .into_iter()
                .map(|o| o.described)
                .collect::<Vec<_>>();
            roots.sort();
            assert_eq!(
                roots,
                [
                    "function ext.f(integer)",
                    "schema ext",
                    "table ext.t",
                    "type ext.r",
                    "view ext.v"
                ],
                "{variable}"
            );
            assert_eq!(failed.statement.line, 1, "{variable}");
            assert!(
                failed.names_something_missing(),
                "{variable}: {}",
                failed.error
            );
        }
    }

    fn managed(tables: &[&str], views: &[&str]) -> crate::resolver::capture::Managed {
        let mut schema = pbps_model::Schema::default();
        for table in tables {
            schema
                .tables
                .insert(table.parse().unwrap(), pbps_model::Table::default());
        }
        for view in views {
            schema.modules.insert(
                view.parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    description: None,
                    definition: String::new(),
                },
            );
        }
        crate::resolver::capture::Managed::from_schema(&schema)
    }

    fn address(described: &str, kind: &str, names: &[&str]) -> Address {
        Address {
            described: described.into(),
            kind: kind.into(),
            names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_chain_is_a_kept_binder_through_an_unmanaged_shape_to_a_managed_relation() {
        let link = |binder: &str, middle: &str| Link {
            binder: address(
                &format!("view {binder}"),
                "view",
                &binder.split('.').collect::<Vec<_>>(),
            ),
            middle: address(
                &format!("table {middle}"),
                "table",
                &middle.split('.').collect::<Vec<_>>(),
            ),
            named: address("table app.m", "table", &["app", "m"]),
        };
        let links = [
            link("app.v", "ext.e"),
            link("app.gone", "ext.e"),
            link("app.v", "app.own"),
        ];
        let desired = managed(&["app.m", "app.own"], &["app.v"]);
        let base = managed(&["app.m", "app.own"], &["app.v", "app.gone"]);
        let chains = chains(&links, &desired, &base);
        assert_eq!(chains.len(), 1, "{chains:?}");
        assert!(chains[0].starts_with(
            "view app.v binds table ext.e, whose shape names the managed table app.m"
        ));
        // Negative: a named relation nobody manages is no chain.
        assert!(super::chains(&links, &managed(&[], &["app.v"]), &managed(&[], &[])).is_empty());
    }

    #[tokio::test]
    #[ignore = "requires the pinned PostgreSQL servers"]
    async fn the_target_records_each_link_through_a_row_type_or_a_parent() {
        use pbps_db::{Conn, Driver};
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
            let name = format!(
                "pbps_link1673_{}",
                crate::catalog::probe_token().replace('-', "_")
            );
            let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
            admin
                .execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
                .await
                .unwrap();
            let mut conn = Conn::connect(Driver::Postgres, &format!("{base} dbname={name}"))
                .await
                .unwrap();
            for statement in [
                "CREATE SCHEMA app",
                "CREATE SCHEMA ext",
                "CREATE TABLE app.m (id int PRIMARY KEY)",
                "CREATE TABLE ext.e (id int, m app.m[])",
                "CREATE TABLE ext.child () INHERITS (app.m)",
                "CREATE FUNCTION ext.f(app.m) RETURNS int LANGUAGE sql RETURN 1",
                "CREATE VIEW app.v AS SELECT id FROM ext.e",
                "CREATE VIEW app.w AS SELECT id FROM ext.child",
                "CREATE VIEW app.x AS SELECT ext.f(m) FROM app.m m",
                // Negative: a plain external table, a view's query and a
                // foreign key make no link.
                "CREATE TABLE ext.plain (id int)",
                "CREATE VIEW app.y AS SELECT id FROM ext.plain",
                "CREATE VIEW ext.over AS SELECT id FROM app.m",
                "CREATE VIEW app.z AS SELECT id FROM ext.over",
                "CREATE TABLE app.fk (e int REFERENCES app.m)",
            ] {
                conn.execute(statement).await.unwrap();
            }
            let links = links(&mut conn).await;
            drop(conn);
            admin
                .execute(&format!("DROP DATABASE {name}"))
                .await
                .unwrap();
            let links = links.unwrap();
            let found: Vec<_> = links
                .iter()
                .map(|l| {
                    (
                        l.binder.described.as_str(),
                        l.middle.described.as_str(),
                        l.named.described.as_str(),
                    )
                })
                .collect();
            assert_eq!(
                found,
                [
                    ("view app.v", "table ext.e", "table app.m"),
                    ("view app.w", "table ext.child", "table app.m"),
                    ("view app.x", "function ext.f(app.m)", "table app.m"),
                ],
                "{variable}"
            );
            assert_eq!(links[2].middle.names, ["ext", "f"], "{variable}");
            assert_eq!(links[0].binder.kind, "view", "{variable}");
        }
    }
}
