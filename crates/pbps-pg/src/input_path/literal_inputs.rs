//! Parse-time input paths must be judged before any original expression binds.
//! PostgreSQL resolves literal parameters; modifier type positions are checked
//! first because typmodin itself runs during Parse (DEC-2378.1).
use pbps_db::{Conn, DbError};
use pbps_model::{TableName, escape::Escaped};

use super::need;

async fn unsafe_types(conn: &mut Conn) -> Result<Vec<(i64, String, String)>, DbError> {
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
                t.typname::pg_catalog.text AS name
           FROM unsafe u JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) u.oid
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace
          ORDER BY t.oid").await?;
    rows.iter()
        .map(|r| {
            Ok((
                need(r, "oid")?,
                need::<&str>(r, "schema")?.into(),
                need::<&str>(r, "name")?.into(),
            ))
        })
        .collect()
}

/// Read both guards before binding even the temporary table's source type.
/// The modifier set is independent of literals; an unreadable set is not empty.
pub(super) async fn before_bind(
    conn: &mut Conn,
    expression: &str,
    source_type: &str,
) -> Result<Option<LiteralInputs>, String> {
    let modifiers = conn
        .query(
            "SELECT t.typname::pg_catalog.text AS name, tn.nspname::pg_catalog.text AS schema
           FROM pg_catalog.pg_type t
           JOIN pg_catalog.pg_namespace tn ON tn.oid OPERATOR(pg_catalog.=) t.typnamespace
           JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) t.typmodin
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
          WHERE n.nspname OPERATOR(pg_catalog.<>) 'pg_catalog' ORDER BY t.oid",
        )
        .await
        .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
    let spans = crate::LEXICON.string_literal_spans(expression);
    if !modifiers.is_empty() {
        // The source is a type declaration, not an expression. Give it the
        // same type-position marker as an expression's explicit cast.
        let source = format!("::{source_type}");
        let tokens = [
            modifier_tokens(expression, &spans),
            modifier_tokens(&source, &crate::LEXICON.string_literal_spans(&source)),
        ];
        for row in &modifiers {
            let name: &str = need(row, "name")
                .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
            let schema: &str = need(row, "schema")
                .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
            if tokens.iter().any(|t| names_modifier(t, schema, name)) {
                return Err(format!(
                    "cannot be bound safely: type `{}.{}` has a modifier routine that is not built into PostgreSQL",
                    Escaped(schema),
                    Escaped(name)
                ));
            }
        }
    }
    let spans: Vec<_> = spans
        .into_iter()
        .filter(|s| !matches!(expression.as_bytes()[s.start], b'b' | b'B' | b'x' | b'X'))
        .collect();
    if spans.is_empty() {
        return Ok(None);
    }
    let types = unsafe_types(conn)
        .await
        .map_err(|_| "could not be vetted: the literal-input catalog could not be read")?;
    if types.is_empty() {
        return Ok(None);
    }
    let mut rewritten = String::new();
    let mut end = 0;
    for (index, span) in spans.iter().enumerate() {
        rewritten.push_str(&expression[end..span.start]);
        rewritten.push_str(&format!(" ${} ", index + 1));
        end = span.end;
    }
    rewritten.push_str(&expression[end..]);
    Ok(Some(LiteralInputs { types, rewritten }))
}

pub(super) struct LiteralInputs {
    types: Vec<(i64, String, String)>,
    rewritten: String,
}

impl LiteralInputs {
    pub(super) async fn check(self, conn: &mut Conn, binding: &TableName) -> Result<(), String> {
        let table = format!(
            "{}.{}",
            crate::quote(&binding.schema).map_err(|e| e.to_string())?,
            crate::quote(&binding.name).map_err(|e| e.to_string())?
        );
        let sql = format!("SELECT (({}) IS NULL) OR true FROM {table}", self.rewritten);
        let prepared = conn.prepare_query(&sql).await.map_err(|e| {
            let code = e.server_error_code().unwrap_or_else(|| "unknown".into());
            format!("could not be vetted: literal parameter preparation failed (SQLSTATE {code}); write the literal as CAST('…' AS type) or '…'::type")
        })?;
        for oid in prepared.parameter_type_ids() {
            if let Some((_, schema, name)) = self.types.iter().find(|t| t.0 == i64::from(oid)) {
                return Err(format!(
                    "cannot be bound safely: a literal resolves to type `{}.{}`, whose input path is not built into PostgreSQL",
                    Escaped(schema),
                    Escaped(name)
                ));
            }
        }
        Ok(())
    }
}

// Only the type positions in gram.y can call typmodin; an ordinary function
// call with a colliding name belongs to the bound-tree audit (DEC-2378.1).
enum ModifierToken {
    Name(String, bool), // The flag preserves quoted spelling, including keywords.
    Cast,
    Symbol(char),
    Literal,
}

fn modifier_tokens(text: &str, spans: &[std::ops::Range<usize>]) -> Vec<ModifierToken> {
    let code = crate::LEXICON.header(text);
    let mut chars = code.char_indices().peekable();
    let mut literals = spans.iter().peekable();
    let mut tokens = Vec::new();
    while let Some((at, ch)) = chars.next() {
        if literals.peek().is_some_and(|s| s.start == at) {
            let span = literals.next().unwrap();
            while chars.next_if(|(at, _)| *at < span.end).is_some() {}
            tokens.push(ModifierToken::Literal);
        } else if ch.is_ascii_whitespace() {
            continue;
        } else if ch == '"' || pbps_dialect::continues_ident(ch) {
            let quoted = ch == '"';
            let mut name = String::new();
            if quoted {
                while let Some((_, c)) = chars.next() {
                    if c == '"' && chars.next_if(|(_, c)| *c == '"').is_none() {
                        break;
                    }
                    name.push(c);
                }
            } else {
                name.push(ch);
                while let Some((_, c)) = chars.next_if(|(_, c)| pbps_dialect::continues_ident(*c)) {
                    name.push(c);
                }
                name.make_ascii_lowercase();
            }
            while name.len() > 63 {
                name.pop();
            }
            tokens.push(ModifierToken::Name(name, quoted));
        } else if ch == ':' && chars.next_if(|(_, c)| *c == ':').is_some() {
            tokens.push(ModifierToken::Cast);
        } else {
            tokens.push(ModifierToken::Symbol(ch));
        }
    }
    tokens
}

fn names_modifier(tokens: &[ModifierToken], schema: &str, name: &str) -> bool {
    for (i, token) in tokens.iter().enumerate() {
        if !matches!(token, ModifierToken::Name(n, _) if n == name)
            || !matches!(tokens.get(i + 1), Some(ModifierToken::Symbol('(')))
        {
            continue;
        }
        let mut start = i;
        while start >= 2
            && matches!(tokens[start - 1], ModifierToken::Symbol('.'))
            && matches!(tokens[start - 2], ModifierToken::Name(..))
        {
            start -= 2;
        }
        if i > start && !matches!(&tokens[i - 2], ModifierToken::Name(s, _) if s == schema) {
            continue;
        }
        if start > 0
            && (matches!(tokens[start - 1], ModifierToken::Cast)
                || matches!(&tokens[start - 1], ModifierToken::Name(s, false) if s == "as" || s == "returning"))
        {
            return true;
        }
        let mut depth = 0;
        for (j, token) in tokens.iter().enumerate().skip(i + 1) {
            match token {
                ModifierToken::Symbol('(') => depth += 1,
                ModifierToken::Symbol(')') => depth -= 1,
                ModifierToken::Name(..)
                | ModifierToken::Cast
                | ModifierToken::Symbol(_)
                | ModifierToken::Literal => {}
            }
            if depth == 0 {
                if matches!(tokens.get(j + 1), Some(ModifierToken::Literal)) {
                    return true;
                }
                break;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{modifier_tokens, names_modifier};

    #[test]
    fn modifiers_are_type_positions_not_function_names() {
        let names = |text| {
            names_modifier(
                &modifier_tokens(text, &crate::LEXICON.string_literal_spans(text)),
                "app",
                "length",
            )
        };
        for text in [
            "x::app.length(1)",
            "CAST(x AS length(1))",
            "TREAT(x AS app.length(1))",
            "XMLSERIALIZE(CONTENT x AS app.length(1))",
            "JSON_ARRAY(RETURNING app.length(1))",
            "app.length((1)) /* gap */ '1'",
            "length(1) E'1'",
            "length(1) $tag$1$tag$",
        ] {
            assert!(names(text), "{text}");
        }
        for text in [
            "length(status)",
            "pg_catalog.length(status)",
            "x::other.length(1)",
            "CAST(x AS pg_catalog.length(1))",
            "x::\"APP\".length(1)",
            "length(')') || '1'",
            "length((1)) + '1'",
            "length(status) /* 'not a literal' */",
            "'length(1)'",
        ] {
            assert!(!names(text), "{text}");
        }
    }
}
