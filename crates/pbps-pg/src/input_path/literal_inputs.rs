//! Parse-time input paths must be judged before any original expression binds.
//! PostgreSQL resolves literal parameters; only modifier syntax needs a coarse
//! name check because typmodin itself runs during Parse (DEC-2378.1).
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
    if !modifiers.is_empty() {
        let code = [
            crate::LEXICON.header(expression),
            crate::LEXICON.header(source_type),
        ];
        for row in &modifiers {
            let name: &str = need(row, "name")
                .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
            let schema: &str = need(row, "schema")
                .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
            if code.iter().any(|s| names_modifier(s, name)) {
                return Err(format!(
                    "cannot be bound safely: type `{}.{}` has a modifier routine that is not built into PostgreSQL",
                    Escaped(schema),
                    Escaped(name)
                ));
            }
        }
    }
    let spans: Vec<_> = crate::LEXICON
        .string_literal_spans(expression)
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

// A name immediately followed by '(' supplies modifiers (or names a function
// already refused by the bound-tree audit). This does not resolve SQL grammar.
// In a qualified name only its last identifier can be followed by '('.
fn names_modifier(code: &str, name: &str) -> bool {
    let mut chars = code.chars().peekable();
    while let Some(ch) = chars.next() {
        let quoted = ch == '"';
        if !quoted && !pbps_dialect::continues_ident(ch) {
            continue;
        }
        let mut token = String::new();
        if quoted {
            while let Some(c) = chars.next() {
                if c == '"' && chars.next_if(|c| *c == '"').is_none() {
                    break;
                }
                token.push(c);
            }
        } else {
            token.push(ch);
            while let Some(c) = chars.next_if(|c| pbps_dialect::continues_ident(*c)) {
                token.push(c);
            }
            token.make_ascii_lowercase();
        }
        while token.len() > 63 {
            token.pop();
        }
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        if token == name && chars.peek() == Some(&'(') {
            return true;
        }
    }
    false
}
