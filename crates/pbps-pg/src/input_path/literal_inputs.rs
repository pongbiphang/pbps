//! Parse-time input paths must be judged before any original expression binds.
//! PostgreSQL resolves literal parameters and name-only type OIDs under the
//! same empty path as execution, before typmodin can run (DEC-2378.1).
use pbps_db::{Conn, DbError, Param};
use pbps_model::{ColumnType, TableName, escape::Escaped};

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
    source_type: Option<&ColumnType>,
) -> Result<Option<LiteralInputs>, String> {
    let modifiers = conn
        .query(
            "SELECT t.oid::pg_catalog.int8 AS oid, t.typname::pg_catalog.text AS name,
                  tn.nspname::pg_catalog.text AS schema
           FROM pg_catalog.pg_type t
           JOIN pg_catalog.pg_namespace tn ON tn.oid OPERATOR(pg_catalog.=) t.typnamespace
           JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) t.typmodin
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
          WHERE n.nspname OPERATOR(pg_catalog.<>) 'pg_catalog' ORDER BY t.oid",
        )
        .await
        .map_err(|_| "could not be vetted: the modifier-input catalog could not be read")?;
    if !modifiers.is_empty() {
        // Strip modifiers structurally: resolving the full source spelling
        // would itself call typmodin. Keep its array identity in the OID.
        let unreadable = "could not be vetted: modifier-input type names could not be resolved";
        let mut oids = Vec::new();
        if let Some(source_type) = source_type {
            let mut source = source_type.clone();
            source.args.clear();
            oids.push(
                resolve_type(conn, &source.raw().to_string())
                    .await
                    .map_err(|_| unreadable)?
                    .ok_or(unreadable)?,
            );
        }
        for name in type_names(expression) {
            if let Some(oid) = resolve_type(conn, &name).await.map_err(|_| unreadable)? {
                oids.push(oid);
            }
        }
        for row in &modifiers {
            let catalog = "could not be vetted: the modifier-input catalog could not be read";
            let oid: i64 = need(row, "oid").map_err(|_| catalog)?;
            let name: &str = need(row, "name").map_err(|_| catalog)?;
            let schema: &str = need(row, "schema").map_err(|_| catalog)?;
            if oids.contains(&oid) {
                return Err(format!(
                    "cannot be bound safely: type `{}.{}` has a modifier routine that is not built into PostgreSQL",
                    Escaped(schema),
                    Escaped(name)
                ));
            }
        }
    }
    let spans = crate::LEXICON.string_literal_spans(expression);
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
    pub(super) async fn check(
        self,
        conn: &mut Conn,
        binding: &TableName,
        predicate: bool,
    ) -> Result<(), String> {
        let table = format!(
            "{}.{}",
            crate::quote(&binding.schema).map_err(|e| e.to_string())?,
            crate::quote(&binding.name).map_err(|e| e.to_string())?
        );
        let check = super::expression_check(&self.rewritten, predicate);
        let sql = format!("SELECT {check} FROM {table}");
        let prepared = conn.prepare_query(&sql).await.map_err(|e| {
            let code = e.server_error_code().unwrap_or_else(|| "unknown".into());
            format!("could not be vetted: literal parameter preparation failed (SQLSTATE {code}); write the literal as CAST('...' AS type) or '...'::type")
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

// Quote every folded name component, including keywords: to_regtype('"as"')
// is an absent identifier, not a syntax error. Never pass a modifier list.
async fn resolve_type(conn: &mut Conn, name: &str) -> Result<Option<i64>, DbError> {
    let rows = conn
        .query_with(
            "SELECT pg_catalog.to_regtype($1)::pg_catalog.oid::pg_catalog.int8 AS oid",
            &[Param::Str(name)],
        )
        .await?;
    rows.first()
        .ok_or_else(|| DbError::Refused("type name lookup returned no row".into()))?
        .try_get("oid")
}

enum NameToken {
    Name(String),
    Dot,
    Other,
}

fn type_names(text: &str) -> std::collections::BTreeSet<String> {
    let code = crate::LEXICON.header(text);
    let mut chars = code.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == '"' || (pbps_dialect::continues_ident(ch) && !ch.is_ascii_digit() && ch != '$') {
            let mut name = String::new();
            if ch == '"' {
                while let Some(c) = chars.next() {
                    if c == '"' && chars.next_if_eq(&'"').is_none() {
                        break;
                    }
                    name.push(c);
                }
            } else {
                name.push(ch);
                while let Some(c) = chars.next_if(|c| pbps_dialect::continues_ident(*c)) {
                    name.push(c);
                }
                name.make_ascii_lowercase();
            }
            tokens.push(NameToken::Name(format!(
                "\"{}\"",
                name.replace('"', "\"\"")
            )));
        } else {
            if ch.is_ascii_digit() || ch == '$' {
                while chars
                    .next_if(|c| pbps_dialect::continues_ident(*c))
                    .is_some()
                {}
            }
            tokens.push(if ch == '.' {
                NameToken::Dot
            } else {
                NameToken::Other
            });
        }
    }
    let mut names = std::collections::BTreeSet::new();
    let mut at = 0;
    while at < tokens.len() {
        if let NameToken::Name(name) = &tokens[at] {
            let mut name = name.clone();
            while let (Some(NameToken::Dot), Some(NameToken::Name(part))) =
                (tokens.get(at + 1), tokens.get(at + 2))
            {
                name.push('.');
                name.push_str(part);
                at += 2;
            }
            names.insert(name);
        }
        at += 1;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::type_names;

    #[test]
    fn type_names_preserve_qualification_and_quote_folded_parts() {
        let names = type_names(
            r#"CAST(x AS SETOF App."Mod""Type"(1)) + pg_catalog.length(x) + db.app.numeric(1)"#,
        );
        for name in [
            r#""cast""#,
            r#""as""#,
            r#""setof""#,
            r#""app"."Mod""Type""#,
            r#""pg_catalog"."length""#,
            r#""db"."app"."numeric""#,
        ] {
            assert!(names.contains(name), "{names:?}: {name}");
        }
        for name in [r#""length""#, r#""numeric""#, r#""1""#] {
            assert!(!names.contains(name), "{names:?}");
        }
        assert!(type_names("'app.unsafe' /* app.modtype */ -- app.modtype\n").is_empty());
        assert!(type_names(r#"app.U&"modt\0079pe""#).contains(r#""app"."modtype""#));
    }
}
