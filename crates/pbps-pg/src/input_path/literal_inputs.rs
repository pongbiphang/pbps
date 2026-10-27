//! Literal input can run during parse analysis, before a bound tree exists.
//! Read the unsafe catalog closure first; inspect text only when it matters
//! (DEC-2378.1). The shared lexer still owns literals, comments and quoting.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError, Param};
use pbps_model::{TableName, escape::Escaped};

use super::{need, oid_array};

async fn unsafe_types(conn: &mut Conn) -> Result<Vec<(i64, String, String, bool)>, DbError> {
    let rows = conn.query(
        "WITH RECURSIVE edges(parent, child) AS (
           SELECT t.oid, t.typbasetype FROM pg_catalog.pg_type t WHERE t.typtype OPERATOR(pg_catalog.=) 'd'
           UNION ALL
           SELECT t.oid, t.typelem FROM pg_catalog.pg_type t JOIN pg_catalog.pg_type e
             ON e.oid OPERATOR(pg_catalog.=) t.typelem AND e.typarray OPERATOR(pg_catalog.=) t.oid
           UNION ALL
           SELECT t.oid, a.atttypid FROM pg_catalog.pg_type t JOIN pg_catalog.pg_attribute a
             ON a.attrelid OPERATOR(pg_catalog.=) t.typrelid
            WHERE t.typtype OPERATOR(pg_catalog.=) 'c' AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped
           UNION ALL
           SELECT r.rngtypid, r.rngsubtype FROM pg_catalog.pg_range r
           UNION ALL
           SELECT r.rngmultitypid, r.rngsubtype FROM pg_catalog.pg_range r
         ), unsafe(oid) AS (
           SELECT t.oid FROM pg_catalog.pg_type t
             JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) t.typinput
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
            WHERE t.typtype OPERATOR(pg_catalog.=) 'b' AND n.nspname OPERATOR(pg_catalog.<>) 'pg_catalog'
           UNION
           SELECT e.parent FROM edges e JOIN unsafe u ON u.oid OPERATOR(pg_catalog.=) e.child
         )
         SELECT t.oid::pg_catalog.int8 AS oid, n.nspname::pg_catalog.text AS schema,
                t.typname::pg_catalog.text AS name, pg_catalog.pg_type_is_visible(t.oid) AS visible
           FROM unsafe u JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) u.oid
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace
          ORDER BY t.oid").await?;
    rows.iter()
        .map(|r| {
            Ok((
                need(r, "oid")?,
                need::<&str>(r, "schema")?.into(),
                need::<&str>(r, "name")?.into(),
                need(r, "visible")?,
            ))
        })
        .collect()
}

pub(super) async fn check(
    conn: &mut Conn,
    table: &TableName,
    expression: &str,
) -> Result<(), String> {
    let unreadable =
        || "could not be vetted: the literal-input catalog could not be read".to_owned();
    let types = unsafe_types(conn).await;
    // An ordinary database pays for no text inspection, and an expression with
    // no literal is never refused by this guard, even with extensions installed.
    if types.as_ref().is_ok_and(Vec::is_empty)
        || crate::LEXICON.string_literals(expression).is_empty()
    {
        return Ok(());
    }
    let types = types.map_err(|_| unreadable())?;
    // Column-name/reserved keywords (CAST, COALESCE, TRIM, ...) are grammar,
    // not bare function-style type casts. Ask this server's keyword catalog.
    let keywords = conn
        .query("SELECT word AS name FROM pg_catalog.pg_get_keywords() WHERE catcode IN ('C', 'R')")
        .await
        .map_err(|_| unreadable())?;
    let keywords: BTreeSet<String> = keywords
        .iter()
        .map(|r| {
            need::<&str>(r, "name")
                .map(str::to_owned)
                .map_err(|_| unreadable())
        })
        .collect::<Result<_, _>>()?;
    let code = crate::LEXICON.header(expression);
    let tokens = tokens(&code);
    let names = names(&tokens);
    let refusal = |schema: &str, name: &str| {
        format!(
            "cannot be bound safely: a literal may reach type `{}.{}`, whose input path is not built into PostgreSQL",
            Escaped(schema),
            Escaped(name)
        )
    };
    for (_, schema, name, visible) in &types {
        for n in names.iter().filter(|n| {
            n.matches(schema, name, *visible)
                && (n.literal_cast(&tokens, &keywords, expression)
                    || n.cast(&tokens)
                    || n.typed_literal(&tokens, expression))
        }) {
            // Name-only lookup, with no modifiers: parseTypeString cannot call
            // an input or typmod routine here. SQL aliases such as integer name
            // built-ins even beside an extension type with that catalog name.
            let target = n.type_name(&tokens);
            let resolved = conn.query_with(
                "SELECT pg_catalog.to_regtype($1::pg_catalog.text)::pg_catalog.oid::pg_catalog.int8 AS oid",
                &[Param::Str(&target)]).await.map_err(|_| unreadable())?;
            let actual = resolved
                .first()
                .ok_or_else(unreadable)?
                .try_get::<i64>("oid")
                .map_err(|_| unreadable())?;
            if actual.is_some_and(|oid| types.iter().any(|t| t.0 == oid)) {
                return Err(refusal(schema, name));
            }
        }
    }
    let oids = oid_array(&types.iter().map(|t| t.0).collect::<Vec<_>>());
    // Every visible overload counts: choosing one would itself need the unsafe
    // bind. Qualified names use exactly their namespace, including hidden ones.
    let rows = conn.query_with(&format!(
        "SELECT DISTINCT 'f'::pg_catalog.text AS kind, n.nspname::pg_catalog.text AS schema,
                p.proname::pg_catalog.text AS name, t.oid::pg_catalog.int8 AS type_oid,
                pg_catalog.pg_function_is_visible(p.oid) AS visible
           FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
           JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) ANY(p.proargtypes)
          WHERE t.oid OPERATOR(pg_catalog.=) ANY({oids})
         UNION
         SELECT 'o', n.nspname::pg_catalog.text, o.oprname::pg_catalog.text, t.oid::pg_catalog.int8,
                pg_catalog.pg_operator_is_visible(o.oid)
           FROM pg_catalog.pg_operator o JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) o.oprnamespace
           JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) o.oprleft OR t.oid OPERATOR(pg_catalog.=) o.oprright
          WHERE t.oid OPERATOR(pg_catalog.=) ANY({oids})
         UNION
         SELECT 'c', n.nspname::pg_catalog.text, a.attname::pg_catalog.text, a.atttypid::pg_catalog.int8, true
           FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
          WHERE n.nspname::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text
            AND c.relname::pg_catalog.text OPERATOR(pg_catalog.=) $2::pg_catalog.text
            AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped
            AND a.atttypid OPERATOR(pg_catalog.=) ANY({oids})
          ORDER BY 1, 2, 3, 4"),
        &[Param::Str(&table.schema), Param::Str(&table.name)])
        .await.map_err(|_| unreadable())?;
    for row in &rows {
        let kind: &str = need(row, "kind").map_err(|_| unreadable())?;
        let schema: &str = need(row, "schema").map_err(|_| unreadable())?;
        let name: &str = need(row, "name").map_err(|_| unreadable())?;
        let visible = need(row, "visible").map_err(|_| unreadable())?;
        let oid: i64 = need(row, "type_oid").map_err(|_| unreadable())?;
        let touched = match kind {
            "f" => {
                names
                    .iter()
                    .any(|n| n.matches(schema, name, visible) && n.called(&tokens, &keywords))
                    || (schema == "pg_catalog" && implied(&tokens, name, false))
            }
            "o" => {
                tokens.iter().enumerate().any(|(i, t)| {
                    !t.ident
                        && t.text == name
                        && if i >= 2 && tokens[i - 1].text == "." {
                            tokens[i - 2].text == schema
                        } else {
                            visible
                        }
                }) || (visible && implied(&tokens, name, true))
            }
            "c" => names
                .iter()
                .any(|n| n.name == name && !n.called(&tokens, &keywords) && !n.cast(&tokens)),
            _ => return Err(unreadable()),
        };
        if touched {
            let (_, schema, name, _) = types.iter().find(|t| t.0 == oid).ok_or_else(unreadable)?;
            return Err(refusal(schema, name));
        }
    }
    Ok(())
}

struct Token {
    text: String,
    ident: bool,
    quoted: bool,
    start: usize,
}

// Only read the code the existing lexer left: no second implementation of
// string, comment, dollar-quote or Unicode-escape rules. Keep delimited names
// whole instead of using may_name's substring matching (#2436).
fn tokens(code: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = code.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let quoted = ch == '"';
        let ident = quoted || pbps_dialect::continues_ident(ch);
        let mut text = String::new();
        if quoted {
            while let Some((_, c)) = chars.next() {
                if c == '"' && chars.next_if(|(_, c)| *c == '"').is_none() {
                    break;
                }
                text.push(c);
            }
        } else {
            text.push(ch);
            while let Some((_, c)) = chars.next_if(|(_, c)| {
                if ident {
                    pbps_dialect::continues_ident(*c)
                } else {
                    operator(ch) && operator(*c)
                }
            }) {
                text.push(c);
            }
            text.make_ascii_lowercase();
        }
        // SQL-standard operators cannot end in + or - when longer than one
        // character. PostgreSQL reads =+ as = followed by unary +, unlike @@+.
        if !ident && operator(ch) && !text.chars().any(|c| "~!@#%^&|`?".contains(c)) {
            let keep = text.trim_end_matches(['+', '-']).len().max(1);
            if keep < text.len() {
                out.push(Token {
                    text: text[..keep].into(),
                    ident: false,
                    quoted: false,
                    start,
                });
                for (i, c) in text[keep..].char_indices() {
                    out.push(Token {
                        text: c.to_string(),
                        ident: false,
                        quoted: false,
                        start: start + keep + i,
                    });
                }
                continue;
            }
        }
        // PostgreSQL clips identifiers to NAMEDATALEN-1, at a character boundary.
        if ident {
            while text.len() > 63 {
                text.pop();
            }
        }
        out.push(Token {
            text,
            ident,
            quoted,
            start,
        });
    }
    out
}

fn operator(c: char) -> bool {
    "+-*/<>=~!@#%^&|`?".contains(c)
}

struct Name<'a> {
    schema: Option<&'a str>,
    name: &'a str,
    first: usize,
    last: usize,
}

fn names(tokens: &[Token]) -> Vec<Name<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !tokens[i].ident {
            i += 1;
            continue;
        }
        let first = i;
        let mut schema = None;
        while i + 2 < tokens.len() && tokens[i + 1].text == "." && tokens[i + 2].ident {
            schema = Some(tokens[i].text.as_str());
            i += 2;
        }
        out.push(Name {
            schema,
            name: &tokens[i].text,
            first,
            last: i,
        });
        i += 1;
    }
    out
}

impl Name<'_> {
    fn type_name(&self, tokens: &[Token]) -> String {
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        if let Some(schema) = self.schema {
            format!("{}.{}", quote(schema), quote(self.name))
        } else if tokens[self.last].quoted {
            quote(self.name)
        } else if self.name == "double"
            && tokens
                .get(self.last + 1)
                .is_some_and(|t| t.text == "precision")
        {
            "double precision".into()
        } else if self.name == "national"
            && tokens
                .get(self.last + 1)
                .is_some_and(|t| ["char", "character"].contains(&t.text.as_str()))
        {
            "national character".into()
        } else {
            self.name.into()
        }
    }
    fn matches(&self, schema: &str, name: &str, visible: bool) -> bool {
        self.name == name && self.schema.map_or(visible, |s| s == schema)
    }
    fn called(&self, tokens: &[Token], keywords: &BTreeSet<String>) -> bool {
        (self.schema.is_some() || tokens[self.last].quoted || !keywords.contains(self.name))
            && tokens.get(self.last + 1).is_some_and(|t| t.text == "(")
    }
    // Function-style casts take an unknown constant. A real function call
    // length(status || 'x') must not be mistaken for a type named length.
    fn literal_cast(
        &self,
        tokens: &[Token],
        keywords: &BTreeSet<String>,
        expression: &str,
    ) -> bool {
        if !self.called(tokens, keywords) {
            return false;
        }
        let start = tokens[self.last + 1].start + 1;
        let mut depth = 1;
        for t in &tokens[self.last + 2..] {
            match t.text.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return crate::LEXICON
                            .string_literals(&expression[start..t.start])
                            .len()
                            == 1;
                    }
                }
                ":" | "." | "unknown" | "pg_catalog" => {}
                _ => return false,
            }
        }
        false
    }
    fn cast(&self, tokens: &[Token]) -> bool {
        self.first.checked_sub(1).is_some_and(|i| {
            tokens[i].text == ":" && i > 0 && tokens[i - 1].text == ":"
                || tokens[i].text == "as" && !tokens[i].quoted
        })
    }
    fn typed_literal(&self, tokens: &[Token], expression: &str) -> bool {
        let end = tokens
            .get(self.last + 1)
            .map_or(expression.len(), |t| t.start);
        !crate::LEXICON
            .string_literals(&expression[tokens[self.last].start..end])
            .is_empty()
    }
}

// SQL grammar spells some calls as words. They still count as named input
// paths, including comparisons introduced by NULLIF, CASE and BETWEEN.
fn implied(tokens: &[Token], name: &str, op: bool) -> bool {
    tokens.iter().enumerate().any(|(i, t)| {
        t.ident
            && !t.quoted
            && (i == 0 || tokens[i - 1].text != ".")
            && match (op, t.text.as_str()) {
                (true, "nullif" | "case" | "in" | "distinct") => name == "=",
                (true, "between") => [">=", "<="].contains(&name),
                (true, "like") => ["~~", "!~~"].contains(&name),
                (true, "ilike") => ["~~*", "!~~*"].contains(&name),
                (true, "similar") => ["~", "!~"].contains(&name),
                (false, "trim") => ["btrim", "ltrim", "rtrim"].contains(&name),
                (false, "position") => name == "strpos",
                (false, "extract" | "substring" | "overlay" | "overlaps" | "normalize") => {
                    name == t.text
                }
                (false, "zone") => {
                    i >= 2
                        && tokens[i - 1].text == "time"
                        && tokens[i - 2].text == "at"
                        && name == "timezone"
                }
                _ => false,
            }
    })
}
