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
pub async fn inventory(
    conn: &mut impl QueryConnection,
) -> Result<BTreeMap<(String, u32), (String, Option<char>)>, DbError> {
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

/// What appeared between two inventories: its roots, and how many extension
/// members came with them.
pub fn created(
    before: &BTreeMap<(String, u32), (String, Option<char>)>,
    after: &BTreeMap<(String, u32), (String, Option<char>)>,
) -> Created {
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
}
